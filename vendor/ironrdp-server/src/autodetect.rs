//! Server-side auto-detect (RTT measurement) per [MS-RDPBCGR 2.2.14].
//!
//! Connect-time detection reports measured RTT and bandwidth to the client.
//! Continuous probes report fresh RTT; results are also exposed via
//! [`AutoDetectManager::snapshot()`].
//!
//! [MS-RDPBCGR 2.2.14]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/dc672839-4f4e-40b1-a71c-cd6a959baa38

use std::collections::VecDeque;
use std::time::Instant;

use anyhow::{Context as _, Result};
use ironrdp_async::{Framed, FramedRead, FramedWrite};
use ironrdp_core::{decode, encode_vec};
use ironrdp_pdu::mcs::{SendDataIndication, SendDataRequest};
use ironrdp_pdu::rdp::autodetect::{
    AutoDetectReqPdu, AutoDetectRequest, AutoDetectResponse, AutoDetectRspPdu, NETCHAR_RESULT_RTT,
};
use ironrdp_pdu::x224::X224;

/// Number of RTT samples to retain for averaging.
const RTT_WINDOW_SIZE: usize = 8;

/// Probes older than this are discarded as unresponsive.
pub(crate) const RTT_PROBE_MAX_AGE: core::time::Duration = core::time::Duration::from_secs(30);

/// Server-side auto-detect state machine.
///
/// Tracks outstanding RTT probes and computes round-trip statistics from
/// client responses. Call [`send_rtt_request()`](Self::send_rtt_request) to
/// generate a probe, then [`handle_response()`](Self::handle_response) when
/// the client replies.
pub struct AutoDetectManager {
    next_sequence: u16,
    pending_probes: Vec<(u16, Instant)>,
    rtt_samples: VecDeque<u32>,
    bandwidth_kbps: Option<u32>,
}

impl AutoDetectManager {
    pub fn new() -> Self {
        Self {
            next_sequence: 0,
            pending_probes: Vec::new(),
            rtt_samples: VecDeque::with_capacity(RTT_WINDOW_SIZE),
            bandwidth_kbps: None,
        }
    }

    /// Generate an RTT Measure Request PDU for continuous detection.
    ///
    /// The caller must encode and send the returned [`AutoDetectRequest`] as
    /// an Auto-Detect Request PDU on the MCS message channel. Timing is tracked
    /// internally by [`AutoDetectManager`].
    pub fn send_rtt_request(&mut self) -> AutoDetectRequest {
        let seq = self.next_sequence;
        self.next_sequence = seq.wrapping_add(1);
        self.pending_probes.push((seq, Instant::now()));
        AutoDetectRequest::rtt_continuous(seq)
    }

    /// Process an RTT Measure Response from the client.
    ///
    /// Returns the measured RTT in milliseconds if the sequence number
    /// matches an outstanding probe, or `None` if it was unexpected.
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "RTT in ms fits in u32 for any plausible network latency"
    )]
    pub fn handle_response(&mut self, response: &AutoDetectResponse) -> Option<u32> {
        let AutoDetectResponse::RttResponse { sequence_number } = response else {
            return None;
        };

        let idx = self.pending_probes.iter().position(|(s, _)| *s == *sequence_number)?;
        let (_, sent_at) = self.pending_probes.remove(idx);

        let rtt_ms = sent_at.elapsed().as_millis() as u32;

        if self.rtt_samples.len() >= RTT_WINDOW_SIZE {
            self.rtt_samples.pop_front();
        }
        self.rtt_samples.push_back(rtt_ms);

        Some(rtt_ms)
    }

    /// Get current RTT statistics, or `None` if no measurements yet.
    pub fn snapshot(&self) -> Option<RttSnapshot> {
        if self.rtt_samples.is_empty() {
            return None;
        }

        let min = *self.rtt_samples.iter().min().unwrap_or(&0);
        let max = *self.rtt_samples.iter().max().unwrap_or(&0);
        let sum: u64 = self.rtt_samples.iter().map(|&v| u64::from(v)).sum();
        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_truncation,
            reason = "average of u32 samples fits in u32"
        )]
        let avg = (sum / self.rtt_samples.len() as u64) as u32;

        Some(RttSnapshot {
            min_ms: min,
            max_ms: max,
            avg_ms: avg,
            sample_count: self.rtt_samples.len(),
        })
    }

    /// Number of outstanding probes awaiting response.
    pub fn pending_count(&self) -> usize {
        self.pending_probes.len()
    }

    /// Discard probes older than the given threshold to prevent unbounded growth.
    pub fn expire_stale_probes(&mut self, max_age: core::time::Duration) {
        self.pending_probes.retain(|(_, sent_at)| sent_at.elapsed() < max_age);
    }

    /// Report a fresh RTT while retaining the connect-time bandwidth estimate.
    pub fn handle_continuous_response(&mut self, response: &AutoDetectResponse) -> Option<AutoDetectRequest> {
        self.handle_response(response)?;
        let rtt = self.snapshot()?;
        let sequence = self.next_sequence;
        self.next_sequence = sequence.wrapping_add(1);
        Some(match self.bandwidth_kbps {
            Some(bandwidth) => AutoDetectRequest::netchar_result(sequence, rtt.min_ms, bandwidth, rtt.avg_ms),
            None => AutoDetectRequest::NetworkCharacteristicsResult {
                sequence_number: sequence,
                request_type: NETCHAR_RESULT_RTT,
                base_rtt_ms: Some(rtt.min_ms),
                bandwidth_kbps: None,
                average_rtt_ms: rtt.avg_ms,
            },
        })
    }

    /// Measure and report network characteristics before licensing. Only call
    /// this when the client advertises SUPPORT_NET_CHAR_AUTODETECT and has joined
    /// the message channel. Missing responses time out without failing login.
    /// Bandwidth uses a bounded 256 KiB burst, never the configured video bitrate.
    pub async fn measure_connect_time<S>(
        &mut self,
        framed: &mut Framed<S>,
        user_channel_id: u16,
        message_channel_id: u16,
    ) -> Result<()>
    where
        S: FramedRead + FramedWrite,
    {
        let sequence = self.send_rtt_request().sequence_number();
        let request = AutoDetectRequest::rtt_connect_time(sequence);
        framed
            .write_all(&encode_request(request, user_channel_id, message_channel_id)?)
            .await
            .context("send connect-time RTT request")?;
        let Some(response) = read_response(framed, message_channel_id, sequence)
            .await
            .context("receive connect-time RTT response")?
        else {
            return Ok(());
        };
        if let AutoDetectResponse::NetworkCharacteristicsSync {
            bandwidth_kbps, rtt_ms, ..
        } = response
        {
            // Auto-reconnect may reuse the client's previous measurements.
            self.pending_probes.clear();
            self.rtt_samples.push_back(rtt_ms);
            self.bandwidth_kbps = Some(bandwidth_kbps);
            let result = AutoDetectRequest::netchar_result(sequence, rtt_ms, bandwidth_kbps, rtt_ms);
            framed
                .write_all(&encode_request(result, user_channel_id, message_channel_id)?)
                .await
                .context("send cached network characteristics")?;
            return Ok(());
        }
        if self.handle_response(&response).is_none() {
            return Ok(());
        }

        let sequence = self.next_sequence;
        self.next_sequence = sequence.wrapping_add(1);
        let start = AutoDetectRequest::bw_start_connect_time(sequence);
        framed
            .write_all(&encode_request(start, user_channel_id, message_channel_id)?)
            .await
            .context("send bandwidth measure start")?;
        // Keep each MCS userData (including security/autodetect headers) below
        // 16 KiB: the IronRDP PER encoder does not implement X.691 fragmentation.
        // A 16 KiB payload produced an invalid 0xC00C length determinant.
        for _ in 0..32 {
            let mut payload = vec![0; 8 * 1024];
            getrandom::getrandom(&mut payload).map_err(|e| anyhow::anyhow!("bandwidth probe random data: {e}"))?;
            let payload = AutoDetectRequest::bw_payload(sequence, payload);
            framed
                .write_all(&encode_request(payload, user_channel_id, message_channel_id)?)
                .await
                .context("send bandwidth measure payload")?;
        }
        let mut payload = vec![0; 4];
        getrandom::getrandom(&mut payload).map_err(|e| anyhow::anyhow!("bandwidth stop random data: {e}"))?;
        let stop = AutoDetectRequest::bw_stop_connect_time(sequence, payload);
        framed
            .write_all(&encode_request(stop, user_channel_id, message_channel_id)?)
            .await
            .context("send bandwidth measure stop")?;

        let response = read_response(framed, message_channel_id, sequence)
            .await
            .context("receive bandwidth measure results")?;
        let rtt = self.snapshot().expect("RTT measured before bandwidth probe");
        let result = match response {
            Some(AutoDetectResponse::NetworkCharacteristicsSync {
                bandwidth_kbps, rtt_ms, ..
            }) => {
                self.rtt_samples.clear();
                self.rtt_samples.push_back(rtt_ms);
                AutoDetectRequest::netchar_result(sequence, rtt_ms, bandwidth_kbps, rtt_ms)
            }
            response => match response.and_then(|r| r.computed_bandwidth_kbps()) {
                Some(bandwidth) => AutoDetectRequest::netchar_result(sequence, rtt.min_ms, bandwidth, rtt.avg_ms),
                None => AutoDetectRequest::NetworkCharacteristicsResult {
                    sequence_number: sequence,
                    request_type: NETCHAR_RESULT_RTT,
                    base_rtt_ms: Some(rtt.min_ms),
                    bandwidth_kbps: None,
                    average_rtt_ms: rtt.avg_ms,
                },
            },
        };
        if let AutoDetectRequest::NetworkCharacteristicsResult { bandwidth_kbps, .. } = &result {
            self.bandwidth_kbps = *bandwidth_kbps;
        }
        framed
            .write_all(&encode_request(result, user_channel_id, message_channel_id)?)
            .await
            .context("send network characteristics result")?;
        Ok(())
    }
}

pub(crate) fn encode_request(request: AutoDetectRequest, user_channel_id: u16, channel_id: u16) -> Result<Vec<u8>> {
    Ok(encode_vec(&X224(SendDataIndication {
        initiator_id: user_channel_id,
        channel_id,
        user_data: encode_vec(&AutoDetectReqPdu::new(request))?.into(),
    }))?)
}

async fn read_response<S>(framed: &mut Framed<S>, channel_id: u16, sequence: u16) -> Result<Option<AutoDetectResponse>>
where
    S: FramedRead,
{
    let deadline = tokio::time::Instant::now() + core::time::Duration::from_secs(2);
    loop {
        let frame = match tokio::time::timeout_at(deadline, framed.read_by_hint(&ironrdp_pdu::X224_HINT)).await {
            Ok(frame) => frame?,
            Err(_) => {
                tracing::debug!("network auto-detect response timed out; continuing connection");
                return Ok(None);
            }
        };
        let data = decode::<X224<SendDataRequest<'_>>>(&frame)?.0;
        if data.channel_id != channel_id {
            anyhow::bail!("unexpected channel during connect-time network detection");
        }
        let response = decode::<AutoDetectRspPdu>(data.user_data.as_ref())?.response;
        if response.sequence_number() == sequence {
            return Ok(Some(response));
        }
    }
}

impl Default for AutoDetectManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Snapshot of RTT measurement results.
#[derive(Debug, Clone, Copy)]
pub struct RttSnapshot {
    /// Minimum observed RTT in milliseconds.
    pub min_ms: u32,
    /// Maximum observed RTT in milliseconds.
    pub max_ms: u32,
    /// Average RTT in milliseconds over the sample window.
    pub avg_ms: u32,
    /// Number of samples in the current window.
    pub sample_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtt_request_increments_sequence() {
        let mut mgr = AutoDetectManager::new();
        let req1 = mgr.send_rtt_request();
        let req2 = mgr.send_rtt_request();
        assert_eq!(req1.sequence_number(), 0);
        assert_eq!(req2.sequence_number(), 1);
        assert_eq!(mgr.pending_count(), 2);
    }

    #[test]
    fn rtt_response_computes_latency() {
        let mut mgr = AutoDetectManager::new();
        let req = mgr.send_rtt_request();

        let response = AutoDetectResponse::RttResponse {
            sequence_number: req.sequence_number(),
        };
        let rtt = mgr.handle_response(&response);
        assert!(rtt.is_some(), "should match the outstanding probe");
        assert_eq!(mgr.pending_count(), 0);
    }

    #[test]
    fn unknown_sequence_returns_none() {
        let mut mgr = AutoDetectManager::new();
        let _ = mgr.send_rtt_request();

        let response = AutoDetectResponse::RttResponse { sequence_number: 999 };
        assert!(mgr.handle_response(&response).is_none());
        assert_eq!(mgr.pending_count(), 1, "original probe should remain");
    }

    #[test]
    fn snapshot_returns_none_without_data() {
        let mgr = AutoDetectManager::new();
        assert!(mgr.snapshot().is_none());
    }

    #[test]
    fn snapshot_reflects_measurements() {
        let mut mgr = AutoDetectManager::new();

        for _ in 0..3 {
            let req = mgr.send_rtt_request();
            let response = AutoDetectResponse::RttResponse {
                sequence_number: req.sequence_number(),
            };
            let _ = mgr.handle_response(&response);
        }

        let snap = mgr.snapshot().expect("should have data after 3 measurements");
        assert_eq!(snap.sample_count, 3);
        assert!(snap.avg_ms < 100);
    }

    #[test]
    fn sequence_number_wraps() {
        let mut mgr = AutoDetectManager::new();
        mgr.next_sequence = u16::MAX;
        let req = mgr.send_rtt_request();
        assert_eq!(req.sequence_number(), u16::MAX);

        let req2 = mgr.send_rtt_request();
        assert_eq!(req2.sequence_number(), 0, "should wrap around");
    }

    #[test]
    fn stale_probe_expiry() {
        let mut mgr = AutoDetectManager::new();
        let _ = mgr.send_rtt_request();
        assert_eq!(mgr.pending_count(), 1);

        mgr.expire_stale_probes(core::time::Duration::ZERO);
        assert_eq!(mgr.pending_count(), 0);
    }
}

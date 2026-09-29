//! Rich-text clipboard codecs: the Windows `HTML Format` (CF_HTML) and
//! `Rich Text Format` payloads, mapped to and from the macOS pasteboard's
//! `public.html` / `public.rtf`.
//!
//! Pure and platform-independent so the fiddly part — CF_HTML's header of
//! **byte** offsets into its own UTF-8 payload — is unit-testable on Linux CI.
//! `clipboard.rs` owns the protocol flow; this module only converts bytes.
//!
//! CF_HTML layout (see the Windows "HTML Clipboard Format" spec):
//!
//! ```text
//! Version:0.9
//! StartHTML:0000000105
//! EndHTML:0000000199
//! StartFragment:0000000141
//! EndFragment:0000000163
//! <html><body>
//! <!--StartFragment-->…<!--EndFragment-->
//! </body></html>
//! ```
//!
//! Every offset counts bytes from the start of the whole buffer, header
//! included. Producers are loose in practice (EndHTML past the end, `-1` for
//! unused fields, a trailing NUL), so decoding is deliberately tolerant.

/// Registered clipboard format name Windows uses for HTML.
pub const HTML_FORMAT_NAME: &str = "HTML Format";
/// Registered clipboard format name Windows uses for RTF.
pub const RTF_FORMAT_NAME: &str = "Rich Text Format";

const START_MARKER: &str = "<!--StartFragment-->";
const END_MARKER: &str = "<!--EndFragment-->";

/// Fixed-width header so its length is known before the offsets are filled
/// in: each offset is always rendered as exactly ten digits.
fn header(start_html: usize, end_html: usize, start_frag: usize, end_frag: usize) -> String {
    format!(
        "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\n\
         StartFragment:{start_frag:010}\r\nEndFragment:{end_frag:010}\r\n"
    )
}

/// Case-insensitive byte search. ASCII lowercasing never changes a byte's
/// length, so indices found in the lowered copy are valid in the original.
fn find_ci(hay: &str, needle: &str, from: usize) -> Option<usize> {
    let hay = hay.as_bytes().get(from..)?.to_ascii_lowercase();
    let needle = needle.as_bytes().to_ascii_lowercase();
    hay.windows(needle.len())
        .position(|w| w == needle.as_slice())
        .map(|i| i + from)
}

fn rfind_ci(hay: &str, needle: &str) -> Option<usize> {
    let hay = hay.as_bytes().to_ascii_lowercase();
    let needle = needle.as_bytes().to_ascii_lowercase();
    hay.windows(needle.len())
        .rposition(|w| w == needle.as_slice())
}

/// Wrap an HTML string from the Mac pasteboard as a Windows CF_HTML payload.
///
/// A full document keeps its `<head>` (Word, Outlook and browsers carry
/// styles there), with the fragment markers placed just inside `<body>`. A
/// bare fragment is wrapped in a minimal document. Existing markers — HTML
/// that already travelled through a Windows clipboard — are reused rather
/// than doubled. The result is NUL-terminated; the offsets exclude the NUL.
pub fn encode_cf_html(html: &str) -> Vec<u8> {
    let reusable = matches!(
        (html.find(START_MARKER), html.rfind(END_MARKER)),
        (Some(start), Some(end)) if start + START_MARKER.len() <= end
    );
    let doc = if reusable {
        html.to_string()
    } else if let (Some(body), Some(body_end)) =
        (find_ci(html, "<body", 0), rfind_ci(html, "</body"))
    {
        match html[body..].find('>').map(|i| body + i + 1) {
            Some(open_end) if open_end <= body_end => format!(
                "{}{START_MARKER}{}{END_MARKER}{}",
                &html[..open_end],
                &html[open_end..body_end],
                &html[body_end..]
            ),
            _ => wrap_fragment(html),
        }
    } else {
        wrap_fragment(html)
    };

    let head_len = header(0, 0, 0, 0).len();
    // Offsets point at the fragment itself: just past the start marker, and
    // at the first byte of the end marker.
    let start_frag = doc.find(START_MARKER).map_or(0, |i| i + START_MARKER.len());
    let end_frag = doc.rfind(END_MARKER).unwrap_or(doc.len());
    let mut out = header(
        head_len,
        head_len + doc.len(),
        head_len + start_frag,
        head_len + end_frag,
    )
    .into_bytes();
    debug_assert_eq!(out.len(), head_len);
    out.extend_from_slice(doc.as_bytes());
    out.push(0);
    out
}

fn wrap_fragment(fragment: &str) -> String {
    format!("<html><body>\r\n{START_MARKER}{fragment}{END_MARKER}\r\n</body></html>")
}

/// Read an offset value, accepting the `-1` producers use for "absent".
fn offset(value: &str) -> Option<usize> {
    value
        .trim()
        .parse::<i64>()
        .ok()
        .and_then(|v| usize::try_from(v).ok())
}

/// Extract the HTML document from a Windows CF_HTML payload.
///
/// Prefers `StartHTML..EndHTML` (the whole document, styles included), falls
/// back to `StartFragment..EndFragment`, and finally to "everything from the
/// first `<`" when the header is missing or its offsets are unusable.
/// Returns `None` only when there is no HTML at all.
pub fn decode_cf_html(data: &[u8]) -> Option<String> {
    let end = data.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let data = &data[..end];
    if data.is_empty() {
        return None;
    }

    // The header is ASCII lines of `Key:Value` before the markup starts.
    let mut start_html = None;
    let mut end_html = None;
    let mut start_frag = None;
    let mut end_frag = None;
    let markup_at = data.iter().position(|&b| b == b'<').unwrap_or(data.len());
    let header_text = String::from_utf8_lossy(&data[..markup_at]);
    for line in header_text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "StartHTML" => start_html = offset(value),
            "EndHTML" => end_html = offset(value),
            "StartFragment" => start_frag = offset(value),
            "EndFragment" => end_frag = offset(value),
            _ => {}
        }
    }

    let slice = |s: Option<usize>, e: Option<usize>| -> Option<&[u8]> {
        let s = s?;
        // Some producers overshoot EndHTML; clamp rather than reject.
        let e = e?.min(data.len());
        (s < e).then(|| &data[s..e])
    };
    let html = slice(start_html, end_html)
        .or_else(|| slice(start_frag, end_frag))
        .or_else(|| (markup_at < data.len()).then(|| &data[markup_at..]))?;
    Some(String::from_utf8_lossy(html).into_owned())
}

/// Wrap RTF bytes from the Mac pasteboard for Windows (NUL-terminated).
pub fn encode_rtf(rtf: &[u8]) -> Vec<u8> {
    let mut out = rtf.to_vec();
    if out.last() != Some(&0) {
        out.push(0);
    }
    out
}

/// Validate and unwrap a Windows `Rich Text Format` payload: trailing NULs
/// removed, and `None` unless it actually starts with an RTF header.
pub fn decode_rtf(data: &[u8]) -> Option<Vec<u8>> {
    let end = data.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let data = &data[..end];
    let start = data.iter().position(|b| !b.is_ascii_whitespace())?;
    data[start..]
        .starts_with(b"{\\rtf")
        .then(|| data[start..].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse the four offsets back out of an encoded header.
    fn offsets(encoded: &[u8]) -> [usize; 4] {
        let text = String::from_utf8_lossy(encoded);
        let get = |k: &str| -> usize {
            let line = text.lines().find(|l| l.starts_with(k)).unwrap();
            line.split_once(':').unwrap().1.trim().parse().unwrap()
        };
        [
            get("StartHTML:"),
            get("EndHTML:"),
            get("StartFragment:"),
            get("EndFragment:"),
        ]
    }

    #[test]
    fn fragment_offsets_are_byte_exact() {
        let enc = encode_cf_html("<b>bold</b>");
        let [sh, eh, sf, ef] = offsets(&enc);
        assert_eq!(&enc[sf..ef], b"<b>bold</b>");
        assert!(enc[sh..eh].starts_with(b"<html>"));
        assert_eq!(eh, enc.len() - 1, "EndHTML excludes the trailing NUL");
        assert_eq!(enc.last(), Some(&0));
    }

    #[test]
    fn offsets_count_bytes_not_characters() {
        // é is 2 bytes, 🙂 is 4 — character-counted offsets would be short.
        let frag = "caf\u{e9} \u{1f642} <i>ok</i>";
        let enc = encode_cf_html(frag);
        let [_, _, sf, ef] = offsets(&enc);
        assert_eq!(&enc[sf..ef], frag.as_bytes());
    }

    #[test]
    fn full_document_keeps_head_and_marks_body() {
        let html = "<html><head><style>p{color:red}</style></head>\
                    <body class=\"x\"><p>hi</p></body></html>";
        let enc = encode_cf_html(html);
        let [sh, eh, sf, ef] = offsets(&enc);
        let doc = std::str::from_utf8(&enc[sh..eh]).unwrap();
        assert!(doc.contains("<style>p{color:red}</style>"));
        assert_eq!(&enc[sf..ef], b"<p>hi</p>");
    }

    #[test]
    fn existing_markers_are_reused_not_doubled() {
        let html = "<html><body><!--StartFragment--><u>u</u><!--EndFragment--></body></html>";
        let enc = encode_cf_html(html);
        let text = String::from_utf8_lossy(&enc);
        assert_eq!(text.matches("<!--StartFragment-->").count(), 1);
        let [_, _, sf, ef] = offsets(&enc);
        assert_eq!(&enc[sf..ef], b"<u>u</u>");
    }

    #[test]
    fn misordered_existing_markers_are_not_trusted() {
        // End marker before the start marker: reusing them would yield
        // StartFragment > EndFragment. Treat the input as a plain fragment.
        let html = "<!--EndFragment-->x<!--StartFragment-->";
        let enc = encode_cf_html(html);
        let [_, _, sf, ef] = offsets(&enc);
        assert!(sf <= ef);
        assert_eq!(&enc[sf..ef], html.as_bytes());
    }

    #[test]
    fn body_tag_matching_is_case_insensitive() {
        let enc = encode_cf_html("<HTML><BODY><p>x</p></BODY></HTML>");
        let [_, _, sf, ef] = offsets(&enc);
        assert_eq!(&enc[sf..ef], b"<p>x</p>");
    }

    #[test]
    fn round_trip_returns_the_document() {
        let html = "<html><body><p>caf\u{e9}</p></body></html>";
        let back = decode_cf_html(&encode_cf_html(html)).unwrap();
        assert!(back.contains("<p>caf\u{e9}</p>"));
    }

    #[test]
    fn decodes_a_windows_style_payload() {
        // Offsets hand-computed for this exact buffer, as Windows writes it
        // (\r\n line endings, a SourceURL line, ten-digit offsets).
        let doc =
            "<html><body>\r\n<!--StartFragment--><b>Hi</b><!--EndFragment-->\r\n</body></html>";
        let head_template = "Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\n\
                             StartFragment:0000000000\r\nEndFragment:0000000000\r\n\
                             SourceURL:https://example.com/\r\n";
        let hl = head_template.len();
        let sf = hl + doc.find("<b>").unwrap();
        let ef = hl + doc.find("<!--EndFragment-->").unwrap();
        let head = format!(
            "Version:0.9\r\nStartHTML:{hl:010}\r\nEndHTML:{:010}\r\n\
             StartFragment:{sf:010}\r\nEndFragment:{ef:010}\r\n\
             SourceURL:https://example.com/\r\n",
            hl + doc.len()
        );
        assert_eq!(head.len(), hl);
        let mut buf = format!("{head}{doc}").into_bytes();
        buf.push(0);
        let html = decode_cf_html(&buf).unwrap();
        assert!(html.starts_with("<html>") && html.ends_with("</html>"));
        assert!(html.contains("<b>Hi</b>"));
    }

    #[test]
    fn overshooting_end_html_is_clamped() {
        let body = "<p>x</p>";
        let head = header(0, 0, 0, 0);
        let hl = head.len();
        let buf = format!("{}{body}", header(hl, hl + 999, hl, hl + body.len()));
        assert_eq!(decode_cf_html(buf.as_bytes()).as_deref(), Some(body));
    }

    #[test]
    fn unusable_offsets_fall_back_to_the_markup() {
        let buf = "Version:0.9\r\nStartHTML:-1\r\nEndHTML:-1\r\n<p>fallback</p>";
        assert_eq!(
            decode_cf_html(buf.as_bytes()).as_deref(),
            Some("<p>fallback</p>")
        );
    }

    #[test]
    fn empty_or_markup_free_payload_is_none() {
        assert_eq!(decode_cf_html(b""), None);
        assert_eq!(decode_cf_html(b"\0\0"), None);
        assert_eq!(decode_cf_html(b"Version:0.9\r\nStartHTML:-1\r\n"), None);
    }

    #[test]
    fn rtf_round_trips_and_is_nul_terminated_for_windows() {
        let rtf = b"{\\rtf1\\ansi {\\b bold}}";
        let enc = encode_rtf(rtf);
        assert_eq!(enc.last(), Some(&0));
        assert_eq!(decode_rtf(&enc).unwrap(), rtf);
        // Already NUL-terminated input isn't double-terminated.
        assert_eq!(encode_rtf(&enc), enc);
    }

    #[test]
    fn non_rtf_payload_is_rejected() {
        assert_eq!(decode_rtf(b"plain text"), None);
        assert_eq!(decode_rtf(b""), None);
        assert_eq!(decode_rtf(b"  {\\rtf1 x}\0").unwrap(), b"{\\rtf1 x}");
    }
}

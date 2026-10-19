use super::*;

#[test]
fn every_byte_boundary_preserves_utf8_multiline_and_crlf() {
    let wire = "event: future\r\ndata: héllo\r\ndata: world\r\n\r\n".as_bytes();
    for split in 0..=wire.len() {
        let mut decoder = Framer::default();
        let mut frames = Vec::new();
        for chunk in [&wire[..split], &wire[split..]] {
            for part in chunk.split_inclusive(|byte| *byte == b'\n') {
                if let Some(frame) = decoder.accept(part).expect("valid UTF8 framing") {
                    frames.push(frame);
                }
            }
        }
        assert_eq!(
            frames,
            vec![Frame {
                event: Some("future".into()),
                data: "héllo\nworld".as_bytes().to_vec()
            }]
        );
    }
}
#[test]
fn comments_and_empty_events_reset_event_names() {
    let mut decoder = Framer::default();
    for line in [
        b":heartbeat\n".as_slice(),
        b"event: ignored\n",
        b"\n",
        b"data: value\n",
    ] {
        assert!(decoder.accept(line).expect("line").is_none());
    }
    assert_eq!(
        decoder.accept(b"\n").expect("frame"),
        Some(Frame {
            event: None,
            data: b"value".to_vec()
        })
    );
}
#[test]
fn invalid_utf8_and_oversized_lines_fail_even_when_delimited() {
    let mut decoder = Framer::default();
    assert!(matches!(
        decoder.accept(b"data: \xff\n"),
        Err(ProviderError::Decode { .. })
    ));
    let mut line = vec![b'x'; MAX_FRAME_BYTES + 1];
    line.push(b'\n');
    assert!(Framer::default().accept(&line).is_err());
}
#[test]
fn multiline_event_budget_is_independent_of_line_budget() {
    let mut decoder = Framer::default();
    let mut line = b"data: ".to_vec();
    line.extend(vec![b'x'; MAX_FRAME_BYTES / 2]);
    line.push(b'\n');
    assert!(decoder.accept(&line).expect("first half").is_none());
    assert!(decoder.accept(&line).is_err());
}

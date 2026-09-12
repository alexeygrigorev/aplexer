#[test]
fn rename_prompt_line_shows_the_caret_and_any_refusal() {
    assert_eq!(rename_prompt_line("", None), "rename: \u{2588}");
    assert_eq!(rename_prompt_line("docs", None), "rename: docs\u{2588}");
    assert_eq!(
        rename_prompt_line("docs", Some("tag may contain only ASCII letters, digits, '.', '_' and '-'")),
        "rename: docs\u{2588}  tag may contain only ASCII letters, digits, '.', '_' and '-'"
    );
}

/// Typing is append-at-the-end in whole chars: split multi-byte UTF-8 must
/// not land as replacement garbage, backspace must remove a whole char (not
/// a byte, or the string turns invalid mid-edit), and Ctrl-u clears.
#[test]
fn rename_prompt_editing_assembles_and_pops_whole_chars() {
    let mut state = RenamePromptState::default();
    let feed = |state: &mut RenamePromptState, bytes: &[u8]| {
        for &b in bytes {
            state.key(b);
        }
    };

    feed(&mut state, b"doc");
    assert_eq!(state.input, "doc");
    // A three-byte char split across feeds only lands when complete.
    feed(&mut state, &[0xe2]);
    assert_eq!(state.input, "doc");
    feed(&mut state, &[0x94]);
    assert_eq!(state.input, "doc");
    feed(&mut state, &[0x81]);
    assert_eq!(state.input, "doc\u{2501}");
    // Backspace pops the whole char.
    state.key(0x7f);
    assert_eq!(state.input, "doc");
    // An invalid sequence is dropped, not parked forever.
    feed(&mut state, &[0xff, b'x']);
    assert_eq!(state.input, "docx");
    // Ctrl-u empties the line.
    state.key(0x15);
    assert!(state.input.is_empty());
}

#[test]
fn rename_prompt_keys_route_by_byte() {
    let mut state = RenamePromptState::default();
    assert!(matches!(state.key(b'\r'), PromptKey::Submit));
    assert!(matches!(state.key(b'\n'), PromptKey::Submit));
    assert!(matches!(state.key(0x1b), PromptKey::Cancel));
    assert!(matches!(state.key(0x03), PromptKey::Cancel));
    assert!(matches!(state.key(0x02), PromptKey::PrefixThenCancel));
    assert!(matches!(state.key(b'a'), PromptKey::Edit));
    // Every other control byte is an ignored edit, not a stray command.
    assert!(matches!(state.key(0x09), PromptKey::Edit));
    assert_eq!(state.input, "a", "Tab must not type into the tag");
}

/// While the prompt is up it owns the bar: it beats the normal status text
/// and a flash, and it is padded to the row width like every other bar.
#[test]
fn status_bar_prefers_the_rename_prompt_over_everything_else() {
    // flash_status forces a bar redraw on the way in, which writes fd 1.
    let _fd1 = FD1_GUARD.lock().unwrap_or_else(PoisonError::into_inner);
    let ctx = status_ctx_for_test(true);
    flash_status(&ctx, "stale switch failure");
    *ctx.prompt.lock().unwrap() = Some("rename: docs\u{2588}".to_string());

    let text = status_bar_text(&ctx, 80);
    assert!(
        text.starts_with("rename: docs"),
        "the prompt must own the row: {text:?}"
    );
    assert!(
        !text.contains("stale"),
        "a flash must not displace the prompt: {text:?}"
    );
    assert_eq!(text.chars().count(), 80, "the row must be padded full-width");

    // Closing the prompt restores the ordinary bar (flash included -- its
    // 3s window is what flash_status set).
    *ctx.prompt.lock().unwrap() = None;
    let text = status_bar_text(&ctx, 80);
    assert!(
        text.contains("stale switch failure"),
        "the flash must come back once the prompt closes: {text:?}"
    );
}

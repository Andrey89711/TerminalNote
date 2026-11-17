//! Audit regressions use temporary stores/configs and injected update channels only.
use super::tests::{app, key, rendered, screen};
use super::*;

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn assert_same_state(left: &App, right: &App) {
    assert_eq!(left.mode, right.mode);
    assert_eq!(
        left.overlay.as_ref().map(|overlay| overlay.kind),
        right.overlay.as_ref().map(|overlay| overlay.kind)
    );
    assert_eq!(left.selected_path(), right.selected_path());
    assert_eq!(left.preview_focus, right.preview_focus);
    assert_eq!(left.history, right.history);
    assert_eq!(left.query, right.query);
    assert_eq!(left.input, right.input);
    assert_eq!(left.status, right.status);
    assert_eq!(left.error, right.error);
    assert_eq!(left.exit, right.exit);
    assert_eq!(left.edited_text(), right.edited_text());
    assert_eq!(left.dirty(), right.dirty());
}

#[test]
fn command_predicate_and_palette_preserve_contextual_membership_and_order() {
    let (_root, mut app) = app();
    for selection in [
        Node::Note("First.md".into()),
        Node::Folder(PathBuf::new()),
        Node::Note("Missing.md".into()),
    ] {
        app.explorer_focus = Some(selection);
        app.load_selected();
        for mode in [
            Mode::Browse,
            Mode::Edit,
            Mode::Search,
            Mode::New,
            Mode::Links,
            Mode::Confirm { quit: false },
            Mode::Confirm { quit: true },
        ] {
            app.mode = mode;
            for history in [false, true] {
                app.history = if history {
                    vec!["Second.txt".into()]
                } else {
                    Vec::new()
                };
                for running in [false, true] {
                    // Keep the fixture sender alive: no HTTP or worker threads.
                    let (_sender, receiver) = std::sync::mpsc::channel();
                    app.update_rx = running.then_some(receiver);
                    for result in [false, true] {
                        app.update_result = result.then_some(UpdateStatus::UpToDate);
                        for overlay in [
                            None,
                            Some(OverlayKind::Help),
                            Some(OverlayKind::Language),
                            Some(OverlayKind::UpdateConsent),
                            Some(OverlayKind::UpdateResult),
                        ] {
                            app.overlay = overlay.map(|kind| Overlay::new(kind, mode));
                            // Independent reference lists lock down the pre-refactor order.
                            let mut expected = match (mode, overlay) {
                                (Mode::Browse, None) => {
                                    let mut commands = vec![
                                        Command::NewNote,
                                        Command::SearchNotes,
                                        Command::Refresh,
                                    ];
                                    if app.document.is_some() {
                                        commands.extend([Command::EditNote, Command::OpenLinks]);
                                    }
                                    if history {
                                        commands.push(Command::Back);
                                    }
                                    commands.extend([
                                        Command::ToggleFocus,
                                        Command::Language,
                                        Command::Help,
                                        Command::Quit,
                                    ]);
                                    commands
                                }
                                (Mode::Edit, None) => vec![
                                    Command::SaveNote,
                                    Command::Back,
                                    Command::Undo,
                                    Command::Redo,
                                    Command::Language,
                                    Command::Help,
                                    Command::Quit,
                                ],
                                _ => Vec::new(),
                            };
                            if !expected.is_empty() {
                                expected.push(Command::CheckUpdates);
                                if result {
                                    expected.push(Command::UpdateResult);
                                }
                            }
                            let available = app.available_commands();
                            assert_eq!(available, expected);
                            for command in PALETTE_COMMAND_ORDER {
                                assert_eq!(
                                    app.command_available(command),
                                    available.contains(&command),
                                    "{mode:?}, {overlay:?}, {command:?}"
                                );
                            }
                            app.palette = None;
                            app.open_palette();
                            assert_eq!(app.palette.is_some(), !available.is_empty());
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn semantic_browse_shortcuts_match_command_dispatch() {
    for (shortcut, command) in [
        (key(KeyCode::Char('r')), Command::Refresh),
        (key(KeyCode::Char('n')), Command::NewNote),
        (key(KeyCode::Char('/')), Command::SearchNotes),
        (key(KeyCode::Char('l')), Command::OpenLinks),
        (key(KeyCode::Char('?')), Command::Help),
        (key(KeyCode::Tab), Command::ToggleFocus),
        (key(KeyCode::BackTab), Command::ToggleFocus),
        (key(KeyCode::Enter), Command::EditNote),
        (
            KeyEvent::new(KeyCode::Left, KeyModifiers::ALT),
            Command::Back,
        ),
        (key(KeyCode::Char('q')), Command::Quit),
        (ctrl('q'), Command::Quit),
    ] {
        let (_left_root, mut left) = app();
        let (_right_root, mut right) = app();
        for app in [&mut left, &mut right] {
            app.history.push("Second.txt".into());
        }
        left.key(shortcut);
        right.execute_command(command);
        assert_same_state(&left, &right);
    }
}

#[test]
fn semantic_editor_shortcuts_match_commands_including_russian_controls_and_conflicts() {
    for (shortcut, command) in [
        (ctrl('z'), Command::Undo),
        (ctrl('я'), Command::Undo),
        (ctrl('y'), Command::Redo),
        (ctrl('н'), Command::Redo),
        (ctrl('s'), Command::SaveNote),
        (ctrl('ы'), Command::SaveNote),
        (key(KeyCode::Esc), Command::Back),
        (ctrl('q'), Command::Quit),
    ] {
        for conflict in [false, true] {
            let (left_root, mut left) = app();
            let (right_root, mut right) = app();
            for (root, app) in [(&left_root, &mut left), (&right_root, &mut right)] {
                app.start_edit();
                app.paste("draft".into());
                if command == Command::Redo {
                    app.execute_command(Command::Undo);
                }
                if conflict {
                    std::fs::write(root.path().join("First.md"), "external").unwrap();
                }
            }
            left.key(shortcut);
            right.execute_command(command);
            assert_same_state(&left, &right);
            assert_eq!(
                std::fs::read(left_root.path().join("First.md")).unwrap(),
                std::fs::read(right_root.path().join("First.md")).unwrap()
            );
        }
    }
}

#[test]
fn one_dirty_comparison_per_frame_tracks_large_document_undo_save_and_conflict() {
    let (root, mut app) = app();
    let original = format!("{}\n", "large document ".repeat(70_000));
    std::fs::write(root.path().join("First.md"), &original).unwrap();
    app.load_selected();
    app.start_edit();
    let draw = |app: &mut App, dirty: bool| {
        app.dirty_comparisons.set(0);
        let text = screen(&rendered(app, 100, 28));
        assert_eq!(app.dirty_comparisons.get(), 1);
        assert_eq!(text.contains(" · unsaved"), dirty);
        assert_eq!(text.contains(" • unsaved"), dirty);
    };
    draw(&mut app, false);
    app.paste("draft".into());
    draw(&mut app, true);
    app.key(ctrl('z'));
    assert_eq!(app.edited_text().unwrap(), original);
    draw(&mut app, false);
    app.key(ctrl('y'));
    draw(&mut app, true);
    app.key(ctrl('s'));
    draw(&mut app, false);
    app.key(ctrl('z')); // Undo after saving is dirty relative to the new baseline.
    draw(&mut app, true);
    app.key(ctrl('y'));
    draw(&mut app, false);
    app.paste("conflicting draft".into());
    std::fs::write(root.path().join("First.md"), "external").unwrap();
    app.key(ctrl('s'));
    assert!(app.error);
    draw(&mut app, true);
    assert_eq!(
        std::fs::read_to_string(root.path().join("First.md")).unwrap(),
        "external"
    );
    app.dirty_comparisons.set(0);
    rendered(&mut app, 40, 8);
    assert_eq!(app.dirty_comparisons.get(), 1);
}

fn overlay_commands() -> [Command; 4] {
    [
        Command::Help,
        Command::Language,
        Command::CheckUpdates,
        Command::UpdateResult,
    ]
}

#[test]
fn overlay_cancel_and_ctrl_q_cancel_preserve_draft_cursor_and_undo() {
    for command in overlay_commands() {
        let (root, mut app) = app();
        app.config_file = Some(root.path().join("config.toml"));
        app.update_result = Some(UpdateStatus::UpToDate);
        app.start_edit();
        let original = app.edited_text();
        app.paste("draft ".into());
        app.key(key(KeyCode::Right));
        let draft = app.edited_text();
        for interrupt in [false, true] {
            let cursor = app.editor.as_ref().unwrap().cursor();
            app.execute_command(command);
            assert_eq!(app.mode, Mode::Edit); // The base mode never becomes a dialog.
            assert_eq!(app.overlay.as_ref().unwrap().return_mode, Mode::Edit);
            rendered(&mut app, 110, 32);
            app.paste("must not enter editor".into());
            app.key(ctrl('s')); // Editor shortcuts are blocked by the overlay.
            app.key(ctrl('p')); // No palette may be opened over an overlay.
            assert!(app.palette.is_none());
            if interrupt {
                app.key(ctrl('q'));
                assert!(app.overlay.is_none());
                assert_eq!(app.mode, Mode::Confirm { quit: true });
                assert!(!app.exit);
            }
            app.key(key(KeyCode::Esc));
            assert!(app.overlay.is_none());
            assert_eq!(app.mode, Mode::Edit);
            assert_eq!(app.edited_text(), draft);
            assert_eq!(app.editor.as_ref().unwrap().cursor(), cursor);
            assert!(app.update_rx.is_none());
            app.key(ctrl('z'));
            assert_eq!(app.edited_text(), original);
            app.key(ctrl('y'));
            assert_eq!(app.edited_text(), draft);
        }
        assert!(!root.path().join("config.toml").exists());
    }
}

#[test]
fn global_quit_from_clean_editor_and_input_modes_never_requires_a_command_palette() {
    for command in overlay_commands() {
        let (_root, mut app) = app();
        app.start_edit();
        app.update_result = Some(UpdateStatus::UpToDate);
        app.execute_command(command);
        app.key(ctrl('q'));
        assert!(app.exit);
        assert!(app.overlay.is_none());
        assert!(app.editor.is_none());
    }
    for mode in [Mode::Search, Mode::New, Mode::Links] {
        let (_root, mut app) = app();
        app.mode = mode;
        app.key(ctrl('q'));
        assert!(app.exit);
    }
    let (_root, mut app) = app();
    app.start_edit();
    app.paste("draft".into());
    app.open_palette();
    app.key(ctrl('q'));
    assert!(app.palette.is_none());
    assert_eq!(app.mode, Mode::Confirm { quit: true });
    app.key(ctrl('q'));
    assert_eq!(app.mode, Mode::Confirm { quit: true });
    assert!(!app.exit);
}

#[test]
fn resizing_clears_overlay_visibility_hits_and_requires_full_consent_again() {
    let (root, mut app) = app();
    app.config_file = Some(root.path().join("config.toml"));
    app.execute_command(Command::Language);
    rendered(&mut app, 110, 32);
    let hit = app.overlay.as_ref().unwrap().hits[1].0;
    rendered(&mut app, 40, 8);
    assert!(!app.overlay.as_ref().unwrap().visible);
    assert!(app.overlay.as_ref().unwrap().hits.is_empty());
    app.mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: hit.x,
        row: hit.y,
        modifiers: KeyModifiers::NONE,
    });
    app.key(key(KeyCode::Down));
    app.key(key(KeyCode::Enter));
    assert_eq!(app.language, Language::English);
    assert!(!root.path().join("config.toml").exists());
    app.key(key(KeyCode::Esc));
    assert!(app.overlay.is_none());
    app.execute_command(Command::Language);
    assert_eq!(app.overlay.as_ref().unwrap().selected, 0);
    assert!(app.overlay.as_ref().unwrap().hits.is_empty());
    app.key(key(KeyCode::Esc));

    // A channel fixture replaces the worker: accepting consent can never make HTTP.
    app.update_worker = || {
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(Ok(UpdateStatus::UpToDate)).unwrap();
        Ok(receiver)
    };
    app.execute_command(Command::CheckUpdates);
    for (width, height) in [(110, 32), (60, 12), (40, 8)] {
        rendered(&mut app, width, height);
    }
    app.key(key(KeyCode::Enter));
    assert!(app.update_rx.is_none());
    assert!(app.overlay.is_some());
    rendered(&mut app, 110, 32);
    assert!(app.overlay.as_ref().unwrap().visible);
    app.key(key(KeyCode::Enter));
    assert!(app.overlay.is_none());
    assert!(app.update_rx.is_some());
    assert!(app.poll_update());
    assert_eq!(app.update_result, Some(UpdateStatus::UpToDate));
    assert!(app.update_rx.is_none());
}

#[test]
fn arriving_update_result_does_not_replace_an_overlay_or_its_return_context() {
    for command in [Command::Help, Command::Language] {
        let (_root, mut app) = app();
        app.start_edit();
        app.paste("draft".into());
        let draft = app.edited_text();
        app.execute_command(command);
        rendered(&mut app, 110, 32);
        let kind = app.overlay.as_ref().unwrap().kind;
        let (sender, receiver) = std::sync::mpsc::channel();
        app.update_rx = Some(receiver);
        sender
            .send(Ok(UpdateStatus::Available("99.0.0".into())))
            .unwrap();
        assert!(app.poll_update());
        assert_eq!(app.overlay.as_ref().unwrap().kind, kind);
        assert_eq!(app.overlay.as_ref().unwrap().return_mode, Mode::Edit);
        app.key(key(KeyCode::Esc));
        assert!(app.overlay.is_none());
        assert_eq!(app.mode, Mode::Edit);
        assert_eq!(app.edited_text(), draft);
        app.execute_command(Command::UpdateResult);
        assert!(screen(&rendered(&mut app, 110, 32)).contains("99.0.0"));
        app.key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Edit);
        assert_eq!(app.edited_text(), draft);
    }
}

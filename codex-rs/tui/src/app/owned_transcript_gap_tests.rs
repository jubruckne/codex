//! The shared context row keeps real editor focus, warning targets, and copy failures coherent.

use super::*;
use crate::history_cell::PlainHistoryCell;
use crossterm::event::MouseButton;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use pretty_assertions::assert_eq;

fn render(app: &mut App, tui: &mut tui::Tui, size: Size) -> Result<String> {
    tui.screen_size_for_event(&TuiEvent::Resize(size))?;
    app.render_owned_transcript(tui, size)?;
    Ok(super::tests::buffer_text(
        crate::custom_terminal::test_support::last_rendered_buffer(&tui.terminal),
    ))
}

fn cursor_row(tui: &tui::Tui, size: Size) -> String {
    let cursor = tui.terminal.last_known_cursor_pos;
    assert!(cursor.x < size.width, "cursor outside terminal: {cursor:?}");
    assert!(
        cursor.y < size.height,
        "cursor outside terminal: {cursor:?}"
    );
    let buffer = crate::custom_terminal::test_support::last_rendered_buffer(&tui.terminal);
    let row = usize::from(cursor.y - buffer.area.y);
    buffer.content()
        [row * usize::from(buffer.area.width)..(row + 1) * usize::from(buffer.area.width)]
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect()
}

fn warning_click(column: u16, row: u16) -> TuiEvent {
    TuiEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

#[tokio::test]
async fn shared_gap_find_and_reverse_search_keep_draft_and_cursor_ownership() -> Result<()> {
    let mut app = crate::app::test_support::make_test_app().await;
    app.local_settings.tui.animations = false;
    let mut server = Box::pin(crate::start_embedded_app_server_for_picker(&app.config)).await?;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    tui.set_owned_screen(true)?;
    app.chat_widget
        .apply_external_edit("draft stays here".into());
    app.transcript_cells = vec![Arc::new(PlainHistoryCell::new(vec![
        "Find this 日本語 needle in the transcript".into(),
    ]))];
    render(&mut app, &mut tui, Size::new(80, 12))?;
    app.transcript_view.begin_search();
    app.transcript_view
        .paste_search(&"日本語 needle ".repeat(12));
    for size in [Size::new(80, 12), Size::new(24, 8), Size::new(12, 8)] {
        let screen = render(&mut app, &mut tui, size)?;
        assert_eq!(
            app.chat_widget.composer_text_with_pending(),
            "draft stays here"
        );
        assert!(app.transcript_view.is_search_editing());
        assert!(cursor_row(&tui, size).contains("Find:"), "{screen}");
        for removed in [
            "copy & follow",
            "for shortcuts",
            "to queue",
            "again to quit",
        ] {
            assert!(!screen.contains(removed));
        }
    }
    assert!(app.handle_owned_transcript_event(
        &mut tui,
        &mut server,
        &TuiEvent::Key(KeyCode::Esc.into()),
    )?);
    assert!(!app.transcript_view.is_search_editing());
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "draft stays here"
    );

    app.handle_tui_event(
        &mut tui,
        &mut server,
        TuiEvent::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
    )
    .await?;
    app.handle_tui_event(
        &mut tui,
        &mut server,
        TuiEvent::Paste("日本語 query tail".repeat(12)),
    )
    .await?;
    for size in [Size::new(80, 12), Size::new(24, 8), Size::new(12, 8)] {
        let screen = render(&mut app, &mut tui, size)?;
        let packet = app
            .chat_widget
            .composer_gap_footer(size.width)
            .expect("reverse search packet");
        assert!(packet.is_interactive);
        let row = cursor_row(&tui, size);
        assert!(
            ["reverse-i-search:", "history:", "? "]
                .iter()
                .any(|prefix| row.contains(prefix)),
            "query cursor must belong to the search row: {screen}"
        );
    }
    app.handle_tui_event(&mut tui, &mut server, TuiEvent::Key(KeyCode::Esc.into()))
        .await?;
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "draft stays here"
    );
    assert!(app.chat_widget.composer_gap_footer(80).is_none());
    server.shutdown().await?;
    tui.set_owned_screen(false)?;
    Ok(())
}

#[tokio::test]
async fn shared_gap_clipped_queries_and_warning_badges_never_paint_outside_the_screen() -> Result<()>
{
    let mut app = crate::app::test_support::make_test_app().await;
    app.local_settings.tui.animations = false;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    tui.set_owned_screen(true)?;
    app.chat_widget.apply_external_edit("retained draft".into());
    app.insert_history_cell(
        &mut tui,
        Box::new(history_cell::new_warning_event("Example diagnostic".into())),
    );
    app.transcript_view.begin_search();
    app.transcript_view.paste_search("日本語 query");
    for width in [1, 8, 24, 80] {
        for height in [1, 2, 3, 4] {
            render(&mut app, &mut tui, Size::new(width, height))?;
            assert_eq!(
                app.chat_widget.composer_text_with_pending(),
                "retained draft"
            );
            assert!(app.transcript_view.is_search_editing());
        }
    }
    tui.set_owned_screen(false)?;
    Ok(())
}

#[tokio::test]
async fn shared_gap_warning_badge_remains_clickable_during_find_and_releases_hidden_targets()
-> Result<()> {
    let mut app = crate::app::test_support::make_test_app().await;
    app.local_settings.tui.animations = false;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    tui.set_owned_screen(true)?;
    app.chat_widget.apply_external_edit("retained draft".into());
    app.insert_history_cell(
        &mut tui,
        Box::new(history_cell::new_warning_event("Example diagnostic".into())),
    );
    app.transcript_view.begin_search();
    app.transcript_view.paste_search("diagnostic");
    let size = Size::new(80, 16);
    render(&mut app, &mut tui, size)?;
    let buffer = crate::custom_terminal::test_support::last_rendered_buffer(&tui.terminal);
    let badge = buffer
        .content()
        .iter()
        .rposition(|cell| cell.symbol() == "⚠")
        .expect("warning badge beside Find");
    let (column, row) = buffer.pos_of(badge);
    assert_eq!(row, tui.terminal.last_known_cursor_pos.y);
    let click = warning_click(column, row);
    assert!(
        app.chat_widget
            .handle_warning_event(&click, &app.transcript_cells)
    );
    assert!(app.chat_widget.keymap_contexts().is_warnings());
    render(&mut app, &mut tui, size)?;
    assert!(
        !app.chat_widget
            .handle_warning_event(&click, &app.transcript_cells),
        "hidden gap must release its target"
    );

    app.chat_widget.handle_key_event(KeyCode::Esc.into());
    app.chat_widget.open_feature_enable_prompt(Feature::Collab);
    render(&mut app, &mut tui, size)?;
    assert!(
        !app.chat_widget
            .handle_warning_event(&click, &app.transcript_cells),
        "a modal must not retain the old badge target"
    );
    assert!(!app.chat_widget.keymap_contexts().is_warnings());
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "retained draft"
    );
    tui.set_owned_screen(false)?;
    Ok(())
}

#[tokio::test]
async fn shared_gap_copy_failure_is_reported_once_with_and_without_a_find_editor() -> Result<()> {
    let mut app = crate::app::test_support::make_test_app().await;
    app.local_settings.tui.animations = false;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    tui.set_owned_screen(true)?;
    app.chat_widget.apply_external_edit("retained draft".into());
    let failed_copy: std::result::Result<crate::clipboard_copy::CopyStatus, String> =
        Err("test clipboard failure".into());
    app.transcript_view.show_copy_feedback(&failed_copy, 12);
    let size = Size::new(80, 12);
    for searching in [false, true] {
        if searching {
            app.transcript_view.begin_search();
            app.transcript_view.paste_search("needle");
        }
        let screen = render(&mut app, &mut tui, size)?;
        assert_eq!(screen.matches("Copy failed").count(), 1, "{screen}");
        assert_eq!(
            app.chat_widget.composer_text_with_pending(),
            "retained draft"
        );
        if searching {
            assert!(cursor_row(&tui, size).contains("Find:"));
        }
    }
    tui.set_owned_screen(false)?;
    Ok(())
}

#[tokio::test]
async fn shared_gap_narrow_find_uses_a_compact_warning_without_wasting_query_space() -> Result<()> {
    let mut app = crate::app::test_support::make_test_app().await;
    app.local_settings.tui.animations = false;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    tui.set_owned_screen(true)?;
    app.chat_widget.apply_external_edit("retained draft".into());
    app.insert_history_cell(
        &mut tui,
        Box::new(history_cell::new_warning_event("Example diagnostic".into())),
    );
    app.transcript_view.begin_search();
    app.transcript_view.paste_search("needle");
    let size = Size::new(32, 12);
    let screen = render(&mut app, &mut tui, size)?;
    let row = cursor_row(&tui, size);
    assert!(row.contains("Find:"), "{screen}");
    assert!(
        row.contains("needle"),
        "a hidden full badge must not consume query columns: {screen}"
    );
    assert!(
        row.contains('⚠'),
        "a compact warning badge fits beside this query: {screen}"
    );
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "retained draft"
    );
    assert!(app.transcript_view.is_search_editing());
    tui.set_owned_screen(false)?;
    Ok(())
}

#[tokio::test]
async fn shared_gap_narrow_find_prioritizes_copy_failure_beside_warning() -> Result<()> {
    let mut app = crate::app::test_support::make_test_app().await;
    app.local_settings.tui.animations = false;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    tui.set_owned_screen(true)?;
    app.chat_widget.apply_external_edit("retained draft".into());
    app.insert_history_cell(
        &mut tui,
        Box::new(history_cell::new_warning_event("Example diagnostic".into())),
    );
    app.transcript_view.begin_search();
    app.transcript_view.paste_search("needle");
    let failed_copy: std::result::Result<crate::clipboard_copy::CopyStatus, String> =
        Err("test clipboard failure".into());
    app.transcript_view.show_copy_feedback(&failed_copy, 12);
    let size = Size::new(40, 12);
    let screen = render(&mut app, &mut tui, size)?;
    let row = cursor_row(&tui, size);
    assert!(row.contains("Find:"), "{screen}");
    assert!(
        row.contains("needle"),
        "query remains usable beside both badges: {screen}"
    );
    assert_eq!(screen.matches("Copy failed").count(), 1, "{screen}");
    assert!(
        row.contains("Copy failed"),
        "copy failure must remain visible while Find owns input: {screen}"
    );
    assert!(
        row.contains('⚠'),
        "the warning can compact after the failure gets its space: {screen}"
    );
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "retained draft"
    );
    assert!(app.transcript_view.is_search_editing());
    tui.set_owned_screen(false)?;
    Ok(())
}

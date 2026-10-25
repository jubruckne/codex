//! A shared floating subagent window, with independently collapsible sections.
use super::*;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Position;
use ratatui::widgets::{Block, BorderType};

const MIN_WIDTH: u16 = 20;
const MIN_HEIGHT: u16 = 6;
const COLLAPSED_HEIGHT: u16 = 3;

#[derive(Default)]
pub(super) struct Windows {
    window: Option<Window>,
    sections: HashMap<ThreadId, Section>,
    order: Vec<ThreadId>,
    painted: Vec<(ThreadId, Rect)>,
    viewport: Rect,
    animation_started: Option<Instant>,
    gesture: Option<Gesture>,
    scroll: u16,
    max_scroll: u16,
    manually_sized: bool,
}
#[derive(Clone, Copy)]
struct Window {
    rect: Rect,
}
impl Window {
    fn clamp(&mut self, area: Rect) {
        self.rect.width = self.rect.width.clamp(MIN_WIDTH.min(area.width), area.width);
        self.rect.height = self
            .rect
            .height
            .clamp(MIN_HEIGHT.min(area.height), area.height);
        self.rect.x = self
            .rect
            .x
            .clamp(area.x, area.right().saturating_sub(self.rect.width));
        self.rect.y = self
            .rect
            .y
            .clamp(area.y, area.bottom().saturating_sub(self.rect.height));
    }
}
struct Section {
    collapsed: bool,
    awaiting_text: bool,
}
#[derive(Clone, Copy)]
struct Edges {
    left: bool,
    right: bool,
    bottom: bool,
}
#[derive(Clone, Copy)]
enum GestureKind {
    Move,
    Resize(Edges),
    Capture,
}
#[derive(Clone, Copy)]
struct Gesture {
    origin: Position,
    rect: Rect,
    kind: GestureKind,
}

impl Windows {
    fn sync_ready(&mut self, area: Rect, agents: &[ThreadId], ready: &HashSet<ThreadId>) {
        if self.viewport != area {
            self.gesture = None;
        }
        self.viewport = area;
        self.order = agents.to_vec();
        self.painted.clear();
        if agents.is_empty() || area.width < 8 || area.height < COLLAPSED_HEIGHT {
            self.order.clear();
            self.gesture = None;
            return;
        }
        for id in agents {
            let section = self.sections.entry(*id).or_insert(Section {
                collapsed: !ready.contains(id),
                awaiting_text: !ready.contains(id),
            });
            if section.awaiting_text && ready.contains(id) {
                section.collapsed = false;
                section.awaiting_text = false;
            }
        }
        let content_height = agents.iter().fold(0u16, |height, id| {
            height.saturating_add(self.section_height(*id))
        });
        let window = self.window.get_or_insert_with(|| {
            let width = (area.width / 4).max(30).saturating_add(8).min(area.width);
            Window {
                rect: Rect::new(
                    area.right() - width,
                    area.y,
                    width,
                    content_height.saturating_add(2).min(area.height),
                ),
            }
        });
        if !self.manually_sized {
            window.rect.height = content_height.saturating_add(2).min(area.height);
        }
        window.clamp(area);
        self.max_scroll = content_height.saturating_sub(window.rect.height.saturating_sub(2));
        self.scroll = self.scroll.min(self.max_scroll);
    }
    fn section_height(&self, id: ThreadId) -> u16 {
        if self.sections[&id].collapsed {
            COLLAPSED_HEIGHT
        } else {
            cards::CARD_HEIGHT
        }
    }
    fn mouse(&mut self, mouse: &MouseEvent) -> bool {
        let position = Position::new(mouse.column, mouse.row);
        if let Some(gesture) = self.gesture {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left) => {
                    self.update_gesture(gesture, position);
                    if matches!(mouse.kind, MouseEventKind::Up(_)) {
                        self.gesture = None;
                    }
                    return true;
                }
                MouseEventKind::Down(_) | MouseEventKind::Up(_) => self.gesture = None,
                _ => return true,
            }
        }
        let Some(window) = self
            .window
            .filter(|window| !self.order.is_empty() && window.rect.contains(position))
        else {
            return false;
        };
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                self.scroll = self.scroll.saturating_add(3).min(self.max_scroll)
            }
            MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(3),
            MouseEventKind::Down(MouseButton::Left) => {
                let rect = window.rect;
                let toggle = self
                    .painted
                    .iter()
                    .find(|(_, header)| {
                        header.contains(position) && position.x == rect.right().saturating_sub(3)
                    })
                    .map(|(id, _)| *id);
                let kind = if let Some(id) = toggle {
                    let section = self.sections.get_mut(&id).unwrap();
                    section.collapsed = !section.collapsed;
                    section.awaiting_text = false;
                    GestureKind::Capture
                } else {
                    let edges = Edges {
                        left: position.x == rect.x,
                        right: position.x == rect.right() - 1,
                        bottom: position.y == rect.bottom() - 1,
                    };
                    if edges.left || edges.right || edges.bottom {
                        self.manually_sized = true;
                        GestureKind::Resize(edges)
                    } else if position.y == rect.y {
                        GestureKind::Move
                    } else {
                        GestureKind::Capture
                    }
                };
                self.gesture = Some(Gesture {
                    origin: position,
                    rect,
                    kind,
                });
            }
            _ => {}
        }
        true
    }
    fn update_gesture(&mut self, gesture: Gesture, position: Position) {
        let Some(window) = self.window.as_mut() else {
            return;
        };
        let dx = i32::from(position.x) - i32::from(gesture.origin.x);
        let dy = i32::from(position.y) - i32::from(gesture.origin.y);
        let area = self.viewport;
        let original = gesture.rect;
        match gesture.kind {
            GestureKind::Move => {
                window.rect.x = (i32::from(original.x) + dx).clamp(
                    i32::from(area.x),
                    i32::from(area.right().saturating_sub(original.width)),
                ) as u16;
                let height = original.height;
                window.rect.y = (i32::from(original.y) + dy).clamp(
                    i32::from(area.y),
                    i32::from(area.bottom().saturating_sub(height)),
                ) as u16;
            }
            GestureKind::Resize(edges) => {
                let min_width = MIN_WIDTH.min(area.width);
                if edges.left {
                    let x = (i32::from(original.x) + dx).clamp(
                        i32::from(area.x),
                        i32::from(original.right().saturating_sub(min_width)),
                    ) as u16;
                    window.rect.x = x;
                    window.rect.width = original.right() - x;
                }
                if edges.right {
                    window.rect.width = (i32::from(original.width) + dx)
                        .clamp(i32::from(min_width), i32::from(area.right() - original.x))
                        as u16;
                }
                if edges.bottom {
                    let min_height = MIN_HEIGHT.min(area.height);
                    window.rect.height = (i32::from(original.height) + dy)
                        .clamp(i32::from(min_height), i32::from(area.bottom() - original.y))
                        as u16;
                }
            }
            GestureKind::Capture => {}
        }
        window.clamp(area);
    }
}

impl RootFeed {
    pub(in crate::app) fn render_windows(
        &self,
        area: Rect,
        buf: &mut Buffer,
        cwd: &Path,
        animation: Option<(&crate::tui::FrameRequester, crate::motion::MotionMode)>,
    ) {
        // A resize can arrive before the terminal's backing buffer catches up.
        let area = area.intersection(buf.area);
        let now = chrono::Utc::now().timestamp();
        let mut agents: Vec<_> = self
            .agents
            .keys()
            .copied()
            .filter(|id| self.card_visible(*id, now))
            .collect();
        agents.sort_by_key(|id| id.to_string());
        let ready: HashSet<_> = agents
            .iter()
            .copied()
            .filter(|id| {
                if self.semantic {
                    self.bubbles
                        .iter()
                        .rev()
                        .find(|message| message.key.0 == *id)
                        .is_some_and(|message| !message.text.trim().is_empty())
                } else {
                    self.windows
                        .get(id)
                        .and_then(semantic::Window::fallback_text)
                        .is_some_and(|text| !text.trim().is_empty())
                }
            })
            .collect();
        let mut windows = self.windows_ui.borrow_mut();
        windows.sync_ready(area, &agents, &ready);
        if windows.order.is_empty() {
            return;
        }
        let started = *windows.animation_started.get_or_insert_with(Instant::now);
        let activity = animation
            .filter(|_| {
                agents.iter().any(|id| {
                    self.cards
                        .get(id)
                        .is_some_and(|state| state.label() == "Working")
                })
            })
            .map(|(requester, progress)| {
                crate::motion::loading_glyph(started, progress, requester)
            });
        let rect = windows.window.unwrap().rect;
        ratatui::widgets::Clear.render(rect, buf);
        let title = format!(" Subagents ({}) ", agents.len());
        let mut border = Block::bordered()
            .border_type(BorderType::Rounded)
            .title(title);
        if windows.max_scroll > 0 {
            border = border.title_bottom(" Scroll ↕ ");
        }
        border.render(rect, buf);
        let inner = Rect::new(
            rect.x + 1,
            rect.y + 1,
            rect.width.saturating_sub(2),
            rect.height.saturating_sub(2),
        );
        let mut offset = 0u16;
        for id in agents {
            let height = windows.section_height(id);
            let collapsed = windows.sections[&id].collapsed;
            // Render a complete section before clipping so scrolling never reflows/truncates it.
            let card = Rect::new(0, 0, rect.width, height);
            let mut section = Buffer::empty(card);
            self.render_agent_card_with_activity(id, card, collapsed, &mut section, cwd, activity);
            for row in 0..height {
                let content_row = offset.saturating_add(row);
                if content_row < windows.scroll {
                    continue;
                }
                let y = inner.y.saturating_add(content_row - windows.scroll);
                if y >= inner.bottom() {
                    break;
                }
                for x in 1..rect.width.saturating_sub(1) {
                    buf[(rect.x + x, y)] = section[(x, row)].clone();
                }
                if row == 0 {
                    windows
                        .painted
                        .push((id, Rect::new(inner.x, y, inner.width, 1)));
                }
            }
            offset = offset.saturating_add(height);
        }
    }
    pub(in crate::app) fn window_areas(&self) -> Vec<Rect> {
        let windows = self.windows_ui.borrow();
        windows
            .window
            .filter(|_| !windows.order.is_empty())
            .map(|window| vec![window.rect])
            .unwrap_or_default()
    }
    pub(super) fn window_mouse(&self, mouse: &MouseEvent) -> bool {
        let now = chrono::Utc::now().timestamp();
        let mut windows = self.windows_ui.borrow_mut();
        windows
            .order
            .retain(|id| self.agents.contains_key(id) && self.card_visible(*id, now));
        let visible: HashSet<_> = windows.order.iter().copied().collect();
        windows.painted.retain(|(id, _)| visible.contains(id));
        if windows.order.is_empty() {
            let captured = windows.gesture.take().is_some();
            return captured
                && matches!(
                    mouse.kind,
                    MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
                );
        }
        windows.mouse(mouse)
    }
    pub(super) fn cancel_window_drag(&self) {
        self.windows_ui.borrow_mut().gesture = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }
    fn feed() -> RootFeed {
        let mut feed = RootFeed::default();
        for value in [1, 2] {
            let id = ThreadId::from_u128(value);
            feed.agents.insert(id, format!("/root/worker_{value}"));
            feed.cards.entry(id).or_default().status =
                codex_app_server_protocol::ThreadStatus::Active {
                    active_flags: vec![],
                };
            feed.upsert(
                MessageKey(id, "turn".into(), "message".into()),
                &format!("I am checking component {value}."),
                true,
                true,
                chrono::Utc::now().timestamp_millis(),
                false,
            );
        }
        feed
    }
    fn render(feed: &RootFeed, height: u16) -> Buffer {
        let area = Rect::new(0, 0, 160, height);
        let mut buf = Buffer::empty(area);
        feed.render_windows(area, &mut buf, Path::new("/tmp"), None);
        buf
    }
    #[test]
    fn agent_windows_share_one_frame_and_keep_agent_sections() {
        let feed = feed();
        let buf = render(&feed, 40);
        assert_eq!(feed.window_areas(), vec![Rect::new(112, 0, 48, 22)]);
        let text = crate::app::owned_transcript::tests::buffer_text(&buf);
        assert_eq!(text.matches('╭').count(), 1);
        assert_eq!(text.matches('╯').count(), 1);
        assert!(text.contains("Subagents (2)"));
        assert!(text.contains("worker_1") && text.contains("worker_2"));
        assert!(text.contains("checking component 1") && text.contains("checking component 2"));
        // Snapshot the shared window, excluding the uncovered transcript area.
        let rect = feed.window_areas()[0];
        let lines = (rect.y..rect.bottom())
            .map(|y| {
                (rect.x..rect.right())
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!("combined_agent_window", lines);
    }
    #[test]
    fn agent_windows_shared_drag_resize_and_hidden_geometry_survive() {
        let feed = feed();
        render(&feed, 40);
        feed.window_mouse(&event(MouseEventKind::Down(MouseButton::Left), 130, 0));
        feed.window_mouse(&event(MouseEventKind::Drag(MouseButton::Left), 30, 5));
        feed.window_mouse(&event(MouseEventKind::Up(MouseButton::Left), 30, 5));
        assert_eq!(feed.window_areas()[0], Rect::new(12, 5, 48, 22));
        feed.window_mouse(&event(MouseEventKind::Down(MouseButton::Left), 59, 26));
        feed.window_mouse(&event(MouseEventKind::Up(MouseButton::Left), 79, 30));
        assert_eq!(feed.window_areas()[0], Rect::new(12, 5, 68, 26));
        let mut ui = feed.windows_ui.borrow_mut();
        ui.sync_ready(Rect::new(0, 0, 160, 40), &[], &HashSet::new());
        assert!(ui.order.is_empty());
        ui.sync_ready(
            Rect::new(0, 0, 160, 40),
            &[ThreadId::from_u128(1)],
            &HashSet::new(),
        );
        assert_eq!(ui.window.unwrap().rect, Rect::new(12, 5, 68, 26));
    }
    #[test]
    fn agent_windows_scroll_overflow_only_inside_shared_frame() {
        let feed = feed();
        render(&feed, 12);
        assert_eq!(feed.window_areas().len(), 1);
        assert!(!feed.window_mouse(&event(MouseEventKind::ScrollDown, 10, 5)));
        for _ in 0..10 {
            assert!(feed.window_mouse(&event(MouseEventKind::ScrollDown, 130, 5)));
        }
        assert_eq!(feed.windows_ui.borrow().scroll, 10);
        let buf = render(&feed, 12);
        let text = crate::app::owned_transcript::tests::buffer_text(&buf);
        assert!(text.contains("worker_2"));
        assert!(text.contains("checking component 2"));
        assert!(!text.contains("worker_1"));
        assert!(text.contains("Scroll"));
        for _ in 0..10 {
            feed.window_mouse(&event(MouseEventKind::ScrollUp, 130, 5));
        }
        assert_eq!(feed.windows_ui.borrow().scroll, 0);
    }
    #[test]
    fn agent_windows_sections_auto_expand_and_respect_manual_collapse() {
        let feed = feed();
        render(&feed, 40);
        let first = ThreadId::from_u128(1);
        feed.window_mouse(&event(MouseEventKind::Down(MouseButton::Left), 157, 1));
        feed.window_mouse(&event(MouseEventKind::Up(MouseButton::Left), 157, 1));
        render(&feed, 40);
        assert!(feed.windows_ui.borrow().sections[&first].collapsed);
        assert_eq!(feed.window_areas()[0].height, 15);
        let second = ThreadId::from_u128(2);
        let mut ui = Windows::default();
        let area = Rect::new(0, 0, 160, 40);
        ui.sync_ready(area, &[first, second], &HashSet::new());
        assert_eq!(ui.window.unwrap().rect.height, 8);
        ui.sync_ready(area, &[first, second], &HashSet::from([first]));
        assert!(!ui.sections[&first].collapsed);
        assert!(ui.sections[&second].collapsed);
        assert_eq!(ui.window.unwrap().rect.height, 15);
    }
    #[test]
    fn agent_windows_empty_or_tiny_viewport_releases_capture_and_targets() {
        let mut feed = feed();
        render(&feed, 40);
        feed.window_mouse(&event(MouseEventKind::Down(MouseButton::Left), 130, 0));
        for card in feed.cards.values_mut() {
            card.removed = true;
        }
        assert!(feed.window_mouse(&event(MouseEventKind::Up(MouseButton::Left), 130, 0)));
        assert!(feed.window_areas().is_empty());
        assert!(!feed.window_mouse(&event(MouseEventKind::ScrollDown, 130, 0)));
        let mut ui = Windows::default();
        ui.sync_ready(
            Rect::new(0, 0, 7, 2),
            &[ThreadId::from_u128(1)],
            &HashSet::new(),
        );
        assert!(ui.order.is_empty());
        assert!(!ui.mouse(&event(MouseEventKind::Down(MouseButton::Left), 0, 0)));
    }
}

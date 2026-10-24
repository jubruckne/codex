//! Floating subagent geometry and pointer capture, independent of agent/model activity.
//! Hidden agents retain session geometry, while only painted windows can receive input.

use super::*;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Position;

const DEFAULT_HEIGHT: u16 = cards::CARD_HEIGHT;
const MIN_WIDTH: u16 = 20;
const MIN_HEIGHT: u16 = 6;
const COLLAPSED_HEIGHT: u16 = 3;

#[derive(Default)]
pub(super) struct Windows {
    states: HashMap<ThreadId, Window>,
    order: Vec<ThreadId>,
    viewport: Rect,
    animation_started: Option<Instant>,
    gesture: Option<Gesture>,
}

#[derive(Clone, Copy, Debug)]
struct Window {
    // Expanded geometry survives collapse; display geometry uses only three rows.
    rect: Rect,
    collapsed: bool,
    awaiting_text: bool,
}
impl Window {
    fn displayed(self) -> Rect {
        Rect {
            height: if self.collapsed {
                COLLAPSED_HEIGHT
            } else {
                self.rect.height
            },
            ..self.rect
        }
    }
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
        self.rect.y = self.rect.y.clamp(
            area.y,
            area.bottom().saturating_sub(self.displayed().height),
        );
    }
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
    Toggle,
}
#[derive(Clone, Copy)]
struct Gesture {
    agent: ThreadId,
    origin: Position,
    rect: Rect,
    kind: GestureKind,
}

impl Windows {
    #[cfg(test)]
    fn sync(&mut self, area: Rect, agents: &[ThreadId]) {
        self.sync_ready(area, agents, &agents.iter().copied().collect());
    }

    fn sync_ready(&mut self, area: Rect, agents: &[ThreadId], ready: &HashSet<ThreadId>) {
        if self.viewport != area {
            self.gesture = None;
        }
        self.viewport = area;
        self.order.retain(|id| agents.contains(id));
        if self
            .gesture
            .is_some_and(|gesture| !self.order.contains(&gesture.agent))
        {
            self.gesture = None;
        }
        if area.width < 8 || area.height < COLLAPSED_HEIGHT {
            self.order.clear();
            self.gesture = None;
            return;
        }
        for id in agents {
            if let Some(window) = self.states.get_mut(id) {
                if window.awaiting_text && ready.contains(id) {
                    window.collapsed = false;
                    window.awaiting_text = false;
                }
                window.clamp(area);
            }
        }
        let mut occupied: Vec<_> = agents
            .iter()
            .filter_map(|id| self.states.get(id))
            .map(|window| window.displayed())
            .collect();
        for (index, id) in agents.iter().enumerate() {
            self.states.entry(*id).or_insert_with(|| {
                let width = (area.width / 4).max(30).saturating_add(8).min(area.width);
                let height = DEFAULT_HEIGHT.min(area.height);
                let rows = (area.height / height).max(1);
                let columns = (area.width / width).max(1);
                let capacity = usize::from(rows) * usize::from(columns);
                let tile = |index: usize| {
                    let slot = index % capacity;
                    let column = slot / usize::from(rows);
                    let row = slot % usize::from(rows);
                    // Overflow cascades slightly, leaving earlier title bars reachable.
                    let cascade = (index / capacity) as u16;
                    let x = area
                        .right()
                        .saturating_sub((column as u16 + 1) * width)
                        .saturating_sub(cascade.saturating_mul(2))
                        .max(area.x);
                    let y = area
                        .y
                        .saturating_add(row as u16 * height)
                        .saturating_add(cascade)
                        .min(area.bottom().saturating_sub(height));
                    Rect::new(x, y, width, height)
                };
                let rect = (0..capacity)
                    .map(tile)
                    .find(|candidate| {
                        occupied.iter().all(|used| {
                            candidate.right() <= used.x
                                || used.right() <= candidate.x
                                || candidate.bottom() <= used.y
                                || used.bottom() <= candidate.y
                        })
                    })
                    .unwrap_or_else(|| tile(index + capacity));
                occupied.push(rect);
                Window {
                    rect,
                    collapsed: !ready.contains(id),
                    awaiting_text: !ready.contains(id),
                }
            });
            if !self.order.contains(id) {
                self.order.push(*id);
            }
            self.states.get_mut(id).unwrap().clamp(area);
        }
    }

    fn hit(&self, position: Position) -> Option<ThreadId> {
        self.order
            .iter()
            .rev()
            .copied()
            .find(|id| self.states[id].displayed().contains(position))
    }

    fn mouse(&mut self, mouse: &MouseEvent) -> bool {
        let position = Position::new(mouse.column, mouse.row);
        if let Some(gesture) = self.gesture {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left) => {
                    if !matches!(gesture.kind, GestureKind::Toggle) {
                        self.update_gesture(gesture, position);
                    }
                    if matches!(mouse.kind, MouseEventKind::Up(_)) {
                        self.gesture = None;
                    }
                    return true;
                }
                MouseEventKind::Down(_) | MouseEventKind::Up(_) => self.gesture = None,
                _ => return true,
            }
        }
        let Some(id) = self.hit(position) else {
            return false;
        };
        if matches!(mouse.kind, MouseEventKind::Down(_)) {
            self.order.retain(|agent| *agent != id);
            self.order.push(id);
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            let window = self.states.get_mut(&id).unwrap();
            let rect = window.displayed();
            let kind = if position.y == rect.y && position.x == rect.right().saturating_sub(3) {
                window.collapsed = !window.collapsed;
                window.awaiting_text = false;
                window.clamp(self.viewport);
                GestureKind::Toggle
            } else {
                let edges = Edges {
                    left: position.x == rect.x,
                    right: position.x == rect.right() - 1,
                    bottom: !window.collapsed && position.y == rect.bottom() - 1,
                };
                if edges.left || edges.right || edges.bottom {
                    GestureKind::Resize(edges)
                } else if position.y == rect.y {
                    GestureKind::Move
                } else {
                    // Capture a body click through release without starting a drag.
                    GestureKind::Toggle
                }
            };
            self.gesture = Some(Gesture {
                agent: id,
                origin: position,
                rect: window.rect,
                kind,
            });
        }
        true
    }

    fn update_gesture(&mut self, gesture: Gesture, position: Position) {
        let Some(window) = self.states.get_mut(&gesture.agent) else {
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
                let height = if window.collapsed {
                    COLLAPSED_HEIGHT
                } else {
                    original.height
                };
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
            GestureKind::Toggle => {}
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
        let (layout, started) = {
            let mut windows = self.windows_ui.borrow_mut();
            let started = *windows.animation_started.get_or_insert_with(Instant::now);
            windows.sync_ready(area, &agents, &ready);
            (
                windows
                    .order
                    .iter()
                    .map(|id| (*id, windows.states[id]))
                    .collect::<Vec<_>>(),
                started,
            )
        };
        let activity = animation
            .filter(|_| {
                layout.iter().any(|(id, _)| {
                    self.cards
                        .get(id)
                        .is_some_and(|state| state.label() == "Working")
                })
            })
            .map(|(requester, progress)| {
                crate::motion::loading_glyph(started, progress, requester)
            });
        for (id, window) in layout {
            self.render_agent_card_with_activity(
                id,
                window.displayed(),
                window.collapsed,
                buf,
                cwd,
                activity,
            );
        }
    }

    pub(in crate::app) fn window_areas(&self) -> Vec<Rect> {
        let windows = self.windows_ui.borrow();
        windows
            .order
            .iter()
            .map(|id| windows.states[id].displayed())
            .collect()
    }

    pub(super) fn window_mouse(&self, mouse: &MouseEvent) -> bool {
        let now = chrono::Utc::now().timestamp();
        let mut windows = self.windows_ui.borrow_mut();
        // Lifecycle events can close a card between painting and the next pointer event.
        // Only retain painted targets; never create an invisible target before the next draw.
        windows
            .order
            .retain(|id| self.agents.contains_key(id) && self.card_visible(*id, now));
        let cancelled_capture = windows
            .gesture
            .is_some_and(|gesture| !windows.order.contains(&gesture.agent));
        if cancelled_capture {
            windows.gesture = None;
            if matches!(
                mouse.kind,
                MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
            ) {
                return true;
            }
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
    fn windows() -> (Windows, ThreadId, ThreadId) {
        let first = ThreadId::from_u128(1);
        let second = ThreadId::from_u128(2);
        let mut windows = Windows::default();
        windows.sync(Rect::new(0, 0, 160, 40), &[first, second]);
        (windows, first, second)
    }
    #[test]
    fn agent_windows_wait_collapsed_then_auto_expand_and_respect_manual_choice() {
        let id = ThreadId::from_u128(1);
        let area = Rect::new(0, 0, 160, 40);
        let mut windows = Windows::default();
        windows.sync_ready(area, &[id], &HashSet::new());
        assert!(windows.states[&id].collapsed);
        assert_eq!(windows.states[&id].displayed().height, 3);
        windows.sync_ready(area, &[id], &HashSet::from([id]));
        assert!(!windows.states[&id].collapsed);
        assert_eq!(windows.states[&id].displayed().height, DEFAULT_HEIGHT);
        windows.mouse(&event(MouseEventKind::Down(MouseButton::Left), 157, 0));
        windows.mouse(&event(MouseEventKind::Up(MouseButton::Left), 157, 0));
        windows.sync_ready(area, &[id], &HashSet::from([id]));
        assert!(
            windows.states[&id].collapsed,
            "new summaries must not undo manual collapse"
        );
        let mut manual = Windows::default();
        manual.sync_ready(area, &[id], &HashSet::new());
        manual.mouse(&event(MouseEventKind::Down(MouseButton::Left), 157, 0));
        manual.mouse(&event(MouseEventKind::Up(MouseButton::Left), 157, 0));
        manual.sync_ready(area, &[id], &HashSet::from([id]));
        assert!(!manual.states[&id].awaiting_text);
        let mut manually_collapsed = Windows::default();
        manually_collapsed.sync_ready(area, &[id], &HashSet::new());
        for _ in 0..2 {
            manually_collapsed.mouse(&event(MouseEventKind::Down(MouseButton::Left), 157, 0));
            manually_collapsed.mouse(&event(MouseEventKind::Up(MouseButton::Left), 157, 0));
        }
        manually_collapsed.sync_ready(area, &[id], &HashSet::from([id]));
        assert!(manually_collapsed.states[&id].collapsed);
    }

    #[test]
    fn agent_windows_tile_down_right_and_preserve_hidden_agent_geometry() {
        let (mut windows, first, second) = windows();
        assert_eq!(windows.states[&first].rect, Rect::new(112, 0, 48, 11));
        assert_eq!(windows.states[&second].rect, Rect::new(112, 11, 48, 11));
        windows.states.get_mut(&first).unwrap().rect.x = 30;
        windows.states.get_mut(&first).unwrap().rect.height = 15;
        windows.sync(windows.viewport, &[second]);
        assert!(!windows.order.contains(&first));
        windows.sync(windows.viewport, &[first, second]);
        assert_eq!(windows.states[&first].rect.x, 30);
        assert_eq!(windows.states[&first].rect.height, 15);
    }
    #[test]
    fn agent_windows_new_agent_uses_vacant_tile_without_covering_existing_window() {
        let (mut windows, first, second) = windows();
        let newcomer = ThreadId::from_u128(0);
        let before = windows.states[&first].rect;
        windows.sync(windows.viewport, &[newcomer, first, second]);
        assert_eq!(windows.states[&first].rect, before);
        assert_eq!(windows.states[&newcomer].rect, Rect::new(112, 22, 48, 11));
    }
    #[test]
    fn agent_windows_collapse_restore_height_and_capture_pointer_release() {
        let (mut windows, first, _) = windows();
        assert!(windows.mouse(&event(MouseEventKind::Down(MouseButton::Left), 157, 0)));
        assert_eq!(windows.states[&first].displayed().height, 3);
        assert_eq!(windows.states[&first].rect.height, 11);
        assert!(windows.mouse(&event(MouseEventKind::Up(MouseButton::Left), 0, 39)));
        assert!(windows.gesture.is_none());
        windows.mouse(&event(MouseEventKind::Down(MouseButton::Left), 157, 0));
        assert_eq!(windows.states[&first].displayed().height, 11);
    }
    #[test]
    fn agent_windows_drag_resize_and_clamp_to_transcript_viewport() {
        let (mut windows, first, _) = windows();
        windows.mouse(&event(MouseEventKind::Down(MouseButton::Left), 130, 0));
        windows.mouse(&event(MouseEventKind::Drag(MouseButton::Left), 10, 39));
        windows.mouse(&event(MouseEventKind::Up(MouseButton::Left), 10, 39));
        assert_eq!(windows.states[&first].rect, Rect::new(0, 29, 48, 11));
        windows.mouse(&event(MouseEventKind::Down(MouseButton::Left), 47, 39));
        windows.mouse(&event(MouseEventKind::Up(MouseButton::Left), 159, 39));
        assert_eq!(windows.states[&first].rect.width, 160);
        windows.sync(Rect::new(0, 0, 18, 8), &[first]);
        assert_eq!(windows.states[&first].rect, Rect::new(0, 0, 18, 8));
        assert!(windows.gesture.is_none());
    }
    #[test]
    fn agent_windows_click_raises_only_hit_window_and_wheel_stays_local() {
        let (mut windows, first, second) = windows();
        windows.states.get_mut(&second).unwrap().rect = Rect::new(120, 2, 40, 12);
        windows.mouse(&event(MouseEventKind::Down(MouseButton::Left), 130, 0));
        windows.mouse(&event(MouseEventKind::Up(MouseButton::Left), 130, 0));
        assert_eq!(windows.order.last(), Some(&first));
        assert_eq!(windows.hit(Position::new(130, 4)), Some(first));
        assert!(windows.mouse(&event(MouseEventKind::ScrollDown, 130, 4)));
        assert!(!windows.mouse(&event(MouseEventKind::ScrollDown, 10, 4)));
    }
    #[test]
    fn agent_windows_closed_after_paint_releases_hit_targets_and_finishes_capture() {
        let feed = RootFeed::default();
        let id = ThreadId::from_u128(1);
        let mut feed = feed;
        feed.agents.insert(id, "worker".into());
        feed.windows_ui
            .borrow_mut()
            .sync(Rect::new(0, 0, 160, 40), &[id]);
        assert!(feed.window_mouse(&event(MouseEventKind::Down(MouseButton::Left), 130, 0)));
        feed.cards.entry(id).or_default().closed = true;
        assert!(feed.window_mouse(&event(MouseEventKind::Up(MouseButton::Left), 130, 0)));
        assert!(feed.windows_ui.borrow().gesture.is_none());
        assert!(!feed.window_mouse(&event(MouseEventKind::Down(MouseButton::Left), 130, 0)));
        assert!(feed.window_areas().is_empty());
    }
    #[test]
    fn agent_windows_tiny_viewport_has_no_hit_targets_and_removal_cancels_drag() {
        let (mut windows, first, second) = windows();
        windows.mouse(&event(MouseEventKind::Down(MouseButton::Left), 130, 0));
        windows.sync(windows.viewport, &[second]);
        assert!(windows.gesture.is_none());
        windows.sync(Rect::new(0, 0, 7, 2), &[first, second]);
        assert!(windows.order.is_empty());
        assert!(!windows.mouse(&event(MouseEventKind::Down(MouseButton::Left), 0, 0)));
    }
}

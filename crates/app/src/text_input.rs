//! A minimal single-line text field entity.
//!
//! Deliberately trimmed vs. gpui's own `examples/input.rs`: no marked text,
//! IME composition, selection, or arrow-key/click caret repositioning --
//! `KeyDownEvent::keystroke.key_char` already gives real, layout-aware typed
//! characters, sufficient for append/backspace-only server URL/username/
//! password entry. The caret is always drawn at the end of `content` since
//! editing never inserts mid-string.

use std::rc::Rc;
use std::time::Duration;

use gpui::{
    div, prelude::*, px, rgb, rgba, App, Context, CursorStyle, FocusHandle, Focusable, IntoElement,
    KeyDownEvent, Render, SharedString, Styled, Window,
};

use crate::theme;

/// Caret blink period -- close to macOS's own text-field caret blink rate.
/// Toggled by a `cx.background_executor()` timer loop spawned once in `new`;
/// `render` only shows the caret when the field is also actually focused.
const CARET_BLINK_INTERVAL: Duration = Duration::from_millis(530);

pub(crate) struct TextInput {
    focus_handle: FocusHandle,
    pub content: String,
    placeholder: SharedString,
    is_password: bool,
    /// Flipped every `CARET_BLINK_INTERVAL`; see the constant's doc comment.
    caret_visible: bool,
    #[allow(clippy::type_complexity)]
    on_enter: Option<Rc<dyn Fn(&mut Window, &mut Context<Self>)>>,
    /// Live-as-you-type signal (300ms debounce, generation-guarded);
    /// fired after `content` actually changes (append
    /// or backspace). Other consumers only read `content` on a later
    /// explicit action.
    #[allow(clippy::type_complexity)]
    on_change: Option<Rc<dyn Fn(&mut Window, &mut Context<Self>)>>,
}

impl TextInput {
    pub(crate) fn new(cx: &mut Context<Self>, placeholder: impl Into<SharedString>) -> Self {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(CARET_BLINK_INTERVAL).await;
                let alive = this
                    .update(cx, |input, cx| {
                        input.caret_visible = !input.caret_visible;
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    // Entity dropped -- stop ticking against a dead weak handle.
                    break;
                }
            }
        })
        .detach();

        Self {
            focus_handle: cx.focus_handle(),
            content: String::new(),
            placeholder: placeholder.into(),
            is_password: false,
            caret_visible: true,
            on_enter: None,
            on_change: None,
        }
    }

    pub(crate) fn password(mut self) -> Self {
        self.is_password = true;
        self
    }

    pub(crate) fn on_change(
        mut self,
        f: impl Fn(&mut Window, &mut Context<Self>) + 'static,
    ) -> Self {
        self.on_change = Some(Rc::new(f));
        self
    }

    /// Marks this field as Tab-cycle stop `index`. GPUI's `Window::
    /// focus_next`/`focus_prev` (bound to Tab/Shift+Tab in `main.rs`) walk
    /// tab stops in ascending `tab_index` order and wrap.
    pub(crate) fn tab_index(mut self, index: isize) -> Self {
        self.focus_handle = self.focus_handle.tab_index(index).tab_stop(true);
        self
    }

    pub(crate) fn on_enter(
        mut self,
        f: impl Fn(&mut Window, &mut Context<Self>) + 'static,
    ) -> Self {
        self.on_enter = Some(Rc::new(f));
        self
    }

    /// Moves GPUI's window focus onto this field.
    pub(crate) fn focus(&self, window: &mut Window) {
        window.focus(&self.focus_handle);
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.keystroke.key.as_str() {
            "backspace" => {
                self.content.pop();
                cx.notify();
                if let Some(handler) = self.on_change.clone() {
                    handler(window, cx);
                }
            }
            "enter" | "return" => {
                if let Some(handler) = self.on_enter.clone() {
                    handler(window, cx);
                }
            }
            _ => {
                if let Some(ch) = &event.keystroke.key_char {
                    if !ch.is_empty() && ch.chars().all(|c| !c.is_control()) {
                        self.content.push_str(ch);
                        cx.notify();
                        if let Some(handler) = self.on_change.clone() {
                            handler(window, cx);
                        }
                    }
                }
            }
        }
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Whether `render` should draw the caret this frame: focused AND on the
/// visible half of the blink cycle. Split out as a pure function so it's
/// unit-testable without a live GPUI window.
fn caret_should_show(focused: bool, blink_visible: bool) -> bool {
    focused && blink_visible
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        let show_caret = caret_should_show(focused, self.caret_visible);
        // 1.5px caret bar, rendered as a flex child at the end of `content`
        // (or before the placeholder when empty) -- see the module doc comment.
        let caret = div()
            .w(px(1.5))
            .h(px(16.))
            .bg(rgba(theme::TEXT_PRIMARY))
            .flex_shrink_0();

        let content_row = if self.content.is_empty() {
            div()
                .flex()
                .items_center()
                .when(show_caret, |d| d.child(caret))
                .child(
                    div()
                        .text_color(rgba(theme::TEXT_QUATERNARY))
                        .child(self.placeholder.clone()),
                )
                .into_any_element()
        } else {
            let display: SharedString = if self.is_password {
                "\u{2022}".repeat(self.content.chars().count()).into()
            } else {
                self.content.clone().into()
            };
            div()
                .flex()
                .items_center()
                .child(div().text_color(rgba(theme::TEXT_PRIMARY)).child(display))
                .when(show_caret, |d| d.child(caret))
                .into_any_element()
        };

        div()
            .id("text-input")
            .track_focus(&self.focus_handle)
            .key_context("TextInput")
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_click(cx.listener(|this, _event, window, cx| {
                window.focus(&this.focus_handle);
                cx.notify();
            }))
            .cursor(CursorStyle::IBeam)
            .w_full()
            .h(px(34.))
            .px_3()
            .flex()
            .items_center()
            .relative()
            .rounded_md()
            .bg(rgb(theme::SURFACE_RAISED))
            // Hairline border stays fixed width so focusing never resizes the box.
            .border_1()
            .border_color(rgb(theme::SURFACE_HAIRLINE))
            .when(focused, |d| {
                d.child(crate::ui::components::focus_ring(theme::RADIUS_CONTROL))
            })
            .child(content_row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins that the caret is hidden whenever unfocused (regardless of blink
    /// phase) and only blinks while focused.
    #[test]
    fn caret_only_shows_while_focused_and_on_the_visible_blink_phase() {
        assert!(caret_should_show(true, true));
        assert!(!caret_should_show(true, false));
        assert!(!caret_should_show(false, true));
        assert!(!caret_should_show(false, false));
    }
}

/// Regression coverage for the crash that pressing Return on the Connect
/// screen used to cause: the process aborted because `on_enter`
/// synchronously re-entered the still-leased `TextInput` entity (GPUI's
/// `EntityMap::lease` panics on double-lease, and a panic inside AppKit's
/// `extern "C"` callback can't unwind, so it aborts instead). Fixed by
/// wrapping the cross-entity update in `cx.defer` in `root.rs`.
/// These tests reproduce the hazard directly against real GPUI entities,
/// using a minimal stand-in for `root::Root` (a real `Root` needs a live
/// AppKit window).
#[cfg(test)]
mod entity_lease_regression {
    use super::*;
    use gpui::{Entity, Keystroke, Modifiers, TestAppContext, WindowHandle};
    use std::cell::RefCell;

    /// Stand-in for `root::Root`: one `TextInput` field plus an `on_submit`
    /// that reads it back.
    struct FakeConnectRoot {
        field: Entity<TextInput>,
        submitted: Rc<RefCell<Option<String>>>,
    }

    impl FakeConnectRoot {
        fn on_submit(&mut self, cx: &mut Context<Self>) {
            let content = self.field.read(cx).content.clone();
            *self.submitted.borrow_mut() = Some(content);
        }
    }

    impl Render for FakeConnectRoot {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            gpui::Empty
        }
    }

    fn char_event(ch: char) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: Keystroke {
                modifiers: Modifiers::default(),
                key: ch.to_string(),
                key_char: Some(ch.to_string()),
            },
            is_held: false,
        }
    }

    fn enter_event() -> KeyDownEvent {
        KeyDownEvent {
            keystroke: Keystroke {
                modifiers: Modifiers::default(),
                key: "enter".to_string(),
                key_char: None,
            },
            is_held: false,
        }
    }

    /// `defer: true` reproduces the fixed (current) code; `defer: false`
    /// reproduces the pre-fix bug, for the negative-control test below.
    fn build_window(
        cx: &mut TestAppContext,
        defer: bool,
    ) -> (WindowHandle<FakeConnectRoot>, Rc<RefCell<Option<String>>>) {
        let submitted = Rc::new(RefCell::new(None));
        let submitted_for_field = submitted.clone();
        let window = cx.add_window(|_window, cx| {
            let root_weak: gpui::WeakEntity<FakeConnectRoot> = cx.entity().downgrade();
            let field = cx.new(|cx| {
                TextInput::new(cx, "placeholder").on_enter(move |_window, cx| {
                    let root_weak = root_weak.clone();
                    if defer {
                        cx.defer(move |cx| {
                            let _ = root_weak.update(cx, |root, cx| root.on_submit(cx));
                        });
                    } else {
                        let _ = root_weak.update(cx, |root, cx| root.on_submit(cx));
                    }
                })
            });
            FakeConnectRoot {
                field,
                submitted: submitted_for_field,
            }
        });
        (window, submitted)
    }

    /// Pins that typing a non-ASCII credential then pressing Return from
    /// inside the field -- the sequence that crashed the Connect screen --
    /// does not panic, and submits exactly what was typed.
    #[gpui::test]
    fn enter_in_a_field_wired_like_connect_screen_does_not_double_lease(cx: &mut TestAppContext) {
        let (window, submitted) = build_window(cx, true);
        let field = window
            .update(cx, |root, _window, _cx| root.field.clone())
            .expect("window just created by build_window should still exist");

        window
            .update(cx, |_root, window, cx| {
                field.update(cx, |input, cx| {
                    for ch in "pässwörd→😀".chars() {
                        input.handle_key_down(&char_event(ch), window, cx);
                    }
                    input.handle_key_down(&enter_event(), window, cx);
                });
            })
            .expect("window just created by build_window should still exist");

        assert_eq!(submitted.borrow().as_deref(), Some("pässwörd→😀"));
    }

    /// Negative control: the undeferred (pre-fix) wiring really does panic
    /// with GPUI's double-lease message.
    #[gpui::test]
    #[should_panic(expected = "already being updated")]
    fn undeferred_submit_reproduces_the_double_lease_panic(cx: &mut TestAppContext) {
        let (window, _submitted) = build_window(cx, false);
        let field = window
            .update(cx, |root, _window, _cx| root.field.clone())
            .expect("window just created by build_window should still exist");

        window
            .update(cx, |_root, window, cx| {
                field.update(cx, |input, cx| {
                    input.handle_key_down(&enter_event(), window, cx);
                });
            })
            .expect("window just created by build_window should still exist");
    }
}

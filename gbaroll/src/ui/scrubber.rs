//! Canvas-based scrub bar (ported from tango's, trimmed): a track with
//! a dimmer fill for the prefetched range, a played fill, and a round
//! playhead handle. Press + drag inside the bar emits the caller's
//! preview message per position change (deduped); release emits the
//! commit message with the last previewed tick. Positions past the
//! prefetch watermark clamp to it, so a click can't stall on unloaded
//! frames.

use iced::widget::canvas::{self, Canvas, Frame, Path, Stroke};
use iced::{mouse, Element, Length, Point, Rectangle, Renderer, Size, Theme};

pub struct Scrubber<M> {
    current: u32,
    total: u32,
    prefetched: u32,
    on_seek: Box<dyn Fn(u32) -> M>,
    on_commit: Box<dyn Fn(u32) -> M>,
    height: f32,
}

#[derive(Default)]
pub struct State {
    dragging: bool,
    /// Last tick published through `on_seek` during this drag, so
    /// repeated cursor moves over the same tick stay silent and the
    /// commit lands exactly on the frame the user last previewed.
    last_emitted: Option<u32>,
}

impl<M> Scrubber<M> {
    pub fn new(
        current: u32,
        total: u32,
        prefetched: u32,
        on_seek: impl Fn(u32) -> M + 'static,
        on_commit: impl Fn(u32) -> M + 'static,
    ) -> Self {
        Self {
            current,
            total,
            prefetched,
            on_seek: Box::new(on_seek),
            on_commit: Box::new(on_commit),
            height: 26.0,
        }
    }

    /// Translate an x within the bar to an absolute tick, clamped to the
    /// prefetched range so a click past the loaded edge doesn't trigger
    /// a long stall while the rest decodes.
    fn tick_at_x(&self, x: f32, width: f32) -> u32 {
        let pct = (x / width.max(1.0)).clamp(0.0, 1.0);
        ((pct * self.total.max(1) as f32).round() as u32).min(self.prefetched)
    }

    pub fn view(self) -> Element<'static, M>
    where
        M: 'static,
    {
        let height = self.height;
        Canvas::new(self)
            .width(Length::Fill)
            .height(Length::Fixed(height))
            .into()
    }
}

impl<M> canvas::Program<M> for Scrubber<M> {
    type State = State;

    fn draw(
        &self,
        state: &State,
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let palette = theme.extended_palette();
        let w = bounds.width;
        let h = bounds.height;
        let total = self.total.max(1) as f32;

        let hovered = state.dragging || cursor.is_over(bounds);
        let handle_r = if hovered { 9.0 } else { 7.0 };

        let track_h = 6.0;
        let track_y = ((h - track_h) / 2.0).round();
        let track_radius = track_h / 2.0;

        let prefetched_w = (self.prefetched as f32 / total).clamp(0.0, 1.0) * w;
        let played_w = (self.current as f32 / total).clamp(0.0, 1.0) * w;

        // Track (unloaded), prefetched underlay, then played fill.
        let track = Path::rounded_rectangle(Point::new(0.0, track_y), Size::new(w, track_h), track_radius.into());
        frame.fill(&track, palette.background.weak.color);
        if prefetched_w > 0.0 {
            let prefetched = Path::rounded_rectangle(
                Point::new(0.0, track_y),
                Size::new(prefetched_w, track_h),
                track_radius.into(),
            );
            frame.fill(&prefetched, palette.primary.weak.color);
        }
        if played_w > 0.0 {
            let played = Path::rounded_rectangle(
                Point::new(0.0, track_y),
                Size::new(played_w, track_h),
                track_radius.into(),
            );
            frame.fill(&played, palette.primary.base.color);
        }

        // Playhead handle, inset so the circle stays inside the canvas.
        let handle_x = played_w.clamp(handle_r, (w - handle_r).max(handle_r));
        let handle = Path::circle(Point::new(handle_x, h / 2.0), handle_r);
        frame.fill(&handle, palette.primary.strong.color);
        frame.stroke(
            &handle,
            Stroke::default().with_color(palette.background.base.color).with_width(2.0),
        );

        vec![frame.into_geometry()]
    }

    fn update(
        &self,
        state: &mut State,
        event: &iced::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<iced::widget::Action<M>> {
        use iced::widget::Action;
        match event {
            iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if let Some(p) = cursor.position_in(bounds) {
                    state.dragging = true;
                    let target = self.tick_at_x(p.x, bounds.width);
                    state.last_emitted = Some(target);
                    return Some(Action::publish((self.on_seek)(target)).and_capture());
                }
            }
            iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if state.dragging {
                    state.dragging = false;
                    // Commit lands on the tick the user last saw
                    // previewed, even when the release happens outside
                    // the bar's bounds.
                    let target = state.last_emitted.take().unwrap_or(self.current);
                    return Some(Action::publish((self.on_commit)(target)).and_capture());
                }
            }
            iced::Event::Mouse(mouse::Event::CursorMoved { .. }) if state.dragging => {
                // Track outside the bar's bounds too, so dragging past
                // either edge clamps to start/end.
                if let Some(raw) = cursor.position() {
                    let relative_x = raw.x - bounds.x;
                    let target = self.tick_at_x(relative_x, bounds.width);
                    if state.last_emitted == Some(target) {
                        return Some(Action::capture());
                    }
                    state.last_emitted = Some(target);
                    return Some(Action::publish((self.on_seek)(target)).and_capture());
                }
            }
            _ => {}
        }
        None
    }

    fn mouse_interaction(&self, state: &State, bounds: Rectangle, cursor: mouse::Cursor) -> mouse::Interaction {
        if state.dragging || cursor.is_over(bounds) {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::default()
        }
    }
}

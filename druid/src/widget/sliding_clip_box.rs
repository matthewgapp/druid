// Copyright 2020 The Druid Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::commands::{self, SCROLL_TO_VIEW, SCROLL_VIEWPORT_CHANGED};
use crate::contexts::ChangeCtx;
use crate::debug_state::DebugState;
use crate::kurbo::{Point, Rect, Size, Vec2};
use crate::widget::prelude::*;
use crate::widget::Axis;
use crate::{platform_menus, Data, InternalLifeCycle, WidgetPod};
use tracing::{info, instrument, trace, warn};

use super::Viewport;

enum Origin {
    Prepared(Point),
    ReadyForCommit(Point),
}

/// Represents the size and position of a rectangular "viewport" into a larger area.
/// A widget exposing a rectangular view into its child, which can be used as a building block for
/// widgets that scroll their child.
pub struct SlidingClipBox<T, W, Y> {
    prev_origin: Point,
    pending_origin: Option<Origin>,
    container: WidgetPod<T, W>,
    sliding_child: WidgetPod<T, Y>,
    port: Viewport,
    constrain_horizontal: bool,
    constrain_vertical: bool,
    must_fill: bool,
    old_bc: BoxConstraints,
    old_size: Size,

    //This ClipBox is wrapped by a widget which manages the viewport_offset
    managed: bool,
}

impl<T, W, Y> SlidingClipBox<T, W, Y>
where
    T: Data,
    W: Widget<T>,
    Y: Widget<T>,
{
    // the thinking here is that we can remember and defer the calculated origin from the layout method
    // so that the next update call can update the data and then commit the origin now that the data has been updated
    // to keep the sliding child in sync with the scroll offset

    // 1. lifecycle is scroll triggers layout
    // 2. layout calculates origin
    // 3. update updates data
    // 4. update readies origin for commit
    // 5. layout commits origin which will be in sync with the sliding child data and the layout of the data

    pub fn prepare_origin(&mut self, origin: Point) {
        self.pending_origin = Some(Origin::Prepared(origin));
    }

    pub fn set_origin(&mut self, ctx: &mut impl ChangeCtx) {
        match self.pending_origin {
            Some(Origin::ReadyForCommit(point)) => {
                self.container.set_origin(ctx, point);
                self.pending_origin = None;
            }
            _ => {
                self.container.set_origin(ctx, self.prev_origin);
            }
        }
    }
}

impl<T, W, Y> SlidingClipBox<T, W, Y> {
    /// Builder-style method for deciding whether to constrain the child vertically.
    ///
    /// The default is `false`.
    ///
    /// This setting affects how a `ClipBox` lays out its child.
    ///
    /// - When it is `false` (the default), the child does not receive any upper
    ///   bound on its height: the idea is that the child can be as tall as it
    ///   wants, and the viewport will somehow get moved around to see all of it.
    /// - When it is `true`, the viewport's maximum height will be passed down
    ///   as an upper bound on the height of the child, and the viewport will set
    ///   its own height to be the same as its child's height.
    pub fn constrain_vertical(mut self, constrain: bool) -> Self {
        self.constrain_vertical = constrain;
        self
    }

    /// Builder-style method for deciding whether to constrain the child horizontally.
    ///
    /// The default is `false`. See [`constrain_vertical`] for more details.
    ///
    /// [`constrain_vertical`]: ClipBox::constrain_vertical
    pub fn constrain_horizontal(mut self, constrain: bool) -> Self {
        self.constrain_horizontal = constrain;
        self
    }

    /// Builder-style method to set whether the child must fill the view.
    ///
    /// If `false` (the default) there is no minimum constraint on the child's
    /// size. If `true`, the child is passed the same minimum constraints as
    /// the `ClipBox`.
    pub fn content_must_fill(mut self, must_fill: bool) -> Self {
        self.must_fill = must_fill;
        self
    }

    /// Returns a reference to the child widget.
    pub fn child(&self) -> &W {
        self.container.widget()
    }

    /// Returns a mutable reference to the child widget.
    pub fn child_mut(&mut self) -> &mut W {
        self.container.widget_mut()
    }

    /// Returns a the viewport describing this `ClipBox`'s position.
    pub fn viewport(&self) -> Viewport {
        self.port
    }

    /// Returns the origin of the viewport rectangle.
    pub fn viewport_origin(&self) -> Point {
        self.port.view_origin
    }

    /// Returns the size of the rectangular viewport into the child widget.
    /// To get the position of the viewport, see [`viewport_origin`].
    ///
    /// [`viewport_origin`]: ClipBox::viewport_origin
    pub fn viewport_size(&self) -> Size {
        self.port.view_size
    }

    /// Returns the size of the child widget.
    pub fn content_size(&self) -> Size {
        self.port.content_size
    }

    /// Set whether to constrain the child horizontally.
    ///
    /// See [`constrain_vertical`] for more details.
    ///
    /// [`constrain_vertical`]: ClipBox::constrain_vertical
    pub fn set_constrain_horizontal(&mut self, constrain: bool) {
        self.constrain_horizontal = constrain;
    }

    /// Set whether to constrain the child vertically.
    ///
    /// See [`constrain_vertical`] for more details.
    ///
    /// [`constrain_vertical`]: ClipBox::constrain_vertical
    pub fn set_constrain_vertical(&mut self, constrain: bool) {
        self.constrain_vertical = constrain;
    }

    /// Set whether the child's size must be greater than or equal the size of
    /// the `ClipBox`.
    ///
    /// See [`content_must_fill`] for more details.
    ///
    /// [`content_must_fill`]: ClipBox::content_must_fill
    pub fn set_content_must_fill(&mut self, must_fill: bool) {
        self.must_fill = must_fill;
    }
}

impl<T, W: Widget<T>, Y: Widget<T>> SlidingClipBox<T, W, Y> {
    /// Creates a new `ClipBox` wrapping `child`.
    ///
    /// This method should only be used when creating your own widget, which uses `ClipBox`
    /// internally.
    ///
    /// `ClipBox` will forward [`SCROLL_TO_VIEW`] notifications to its parent unchanged.
    /// In this case the parent has to handle said notification itself. By default the `ClipBox`
    /// will filter out [`SCROLL_TO_VIEW`] notifications which refer to areas not visible.
    ///
    /// [`SCROLL_TO_VIEW`]: crate::commands::SCROLL_TO_VIEW
    pub fn managed(containing_child: W, sliding_child: Y) -> Self {
        SlidingClipBox {
            container: WidgetPod::new(containing_child),
            sliding_child: WidgetPod::new(sliding_child),
            port: Default::default(),
            constrain_horizontal: false,
            constrain_vertical: false,
            must_fill: false,
            old_bc: BoxConstraints::tight(Size::ZERO),
            old_size: Size::ZERO,
            managed: true,
        }
    }

    /// Creates a new unmanaged `ClipBox` wrapping `child`.
    ///
    /// This method should be used when you are using `ClipBox` in the widget tree directly.
    pub fn unmanaged(containing_child: W, sliding_child: Y) -> Self {
        SlidingClipBox {
            container: WidgetPod::new(containing_child),
            sliding_child: WidgetPod::new(sliding_child),
            port: Default::default(),
            constrain_horizontal: false,
            constrain_vertical: false,
            must_fill: false,
            old_bc: BoxConstraints::tight(Size::ZERO),
            old_size: Size::ZERO,
            managed: false,
        }
    }

    /// Pans by `delta` units.
    ///
    /// Returns `true` if the scroll offset has changed.
    pub fn pan_by<C: ChangeCtx>(&mut self, ctx: &mut C, delta: Vec2) -> bool {
        self.with_port(ctx, |_, port| {
            port.pan_by(delta);
        })
    }

    /// Pans the minimal distance to show the `region`.
    ///
    /// If the target region is larger than the viewport, we will display the
    /// portion that fits, prioritizing the portion closest to the origin.
    pub fn pan_to_visible<C: ChangeCtx>(&mut self, ctx: &mut C, region: Rect) -> bool {
        self.with_port(ctx, |_, port| {
            port.pan_to_visible(region);
        })
    }

    /// Pan to this position on a particular axis.
    ///
    /// Returns `true` if the scroll offset has changed.
    pub fn pan_to_on_axis<C: ChangeCtx>(&mut self, ctx: &mut C, axis: Axis, position: f64) -> bool {
        self.with_port(ctx, |_, port| {
            port.pan_to_on_axis(axis, position);
        })
    }

    /// Modify the `ClipBox`'s viewport rectangle with a closure.
    ///
    /// The provided callback function can modify its argument, and when it is
    /// done then this `ClipBox` will be modified to have the new viewport rectangle.
    pub fn with_port<C: ChangeCtx, F: FnOnce(&mut C, &mut Viewport)>(
        &mut self,
        ctx: &mut C,
        f: F,
    ) -> bool {
        f(ctx, &mut self.port);
        self.port.sanitize_view_origin();
        let new_content_origin = (Point::ZERO - self.port.view_origin).to_point();

        if new_content_origin != self.container.layout_rect().origin() {
            // self.container.set_origin(ctx, new_content_origin);
            true
        } else {
            false
        }
    }
}

impl<T: Data, W: Widget<T>, Y: Widget<T>> Widget<T> for SlidingClipBox<T, W, Y> {
    #[instrument(name = "ClipBox", level = "trace", skip(self, ctx, event, data, env))]
    fn event(&mut self, ctx: &mut EventCtx, event: &Event, data: &mut T, env: &Env) {
        if let Event::Notification(notification) = event {
            if let Some(global_highlight_rect) = notification.get(SCROLL_TO_VIEW) {
                if !self.managed {
                    // If the parent widget does not handle SCROLL_TO_VIEW notifications, we
                    // prevent unexpected behaviour, by clipping SCROLL_TO_VIEW notifications
                    // to this ClipBox's viewport.
                    ctx.set_handled();
                    self.with_port(ctx, |ctx, port| {
                        port.fixed_scroll_to_view_handling(
                            ctx,
                            *global_highlight_rect,
                            notification.source(),
                        );
                    });
                }
            }
        } else {
            self.container.event(ctx, event, data, env);
        }
    }

    #[instrument(name = "ClipBox", level = "trace", skip(self, ctx, event, data, env))]
    fn lifecycle(&mut self, ctx: &mut LifeCycleCtx, event: &LifeCycle, data: &T, env: &Env) {
        match event {
            LifeCycle::ViewContextChanged(view_context) => {
                let mut view_context = *view_context;
                view_context.clip = view_context.clip.intersect(ctx.size().to_rect());
                let modified_event = LifeCycle::ViewContextChanged(view_context);
                self.container.lifecycle(ctx, &modified_event, data, env);
            }
            LifeCycle::Internal(InternalLifeCycle::RouteViewContextChanged(view_context)) => {
                let mut view_context = *view_context;
                view_context.clip = view_context.clip.intersect(ctx.size().to_rect());
                let modified_event =
                    LifeCycle::Internal(InternalLifeCycle::RouteViewContextChanged(view_context));
                self.container.lifecycle(ctx, &modified_event, data, env);
            }
            _ => {
                self.container.lifecycle(ctx, event, data, env);
            }
        }
    }

    #[instrument(
        name = "ClipBox",
        level = "trace",
        skip(self, ctx, _old_data, data, env)
    )]
    fn update(&mut self, ctx: &mut UpdateCtx, _old_data: &T, data: &T, env: &Env) {
        self.container.update(ctx, data, env);
    }

    #[instrument(name = "ClipBox", level = "trace", skip(self, ctx, bc, data, env))]
    fn layout(&mut self, ctx: &mut LayoutCtx, bc: &BoxConstraints, data: &T, env: &Env) -> Size {
        bc.debug_check("ClipBox");

        let max_child_width = if self.constrain_horizontal {
            bc.max().width
        } else {
            f64::INFINITY
        };
        let max_child_height = if self.constrain_vertical {
            bc.max().height
        } else {
            f64::INFINITY
        };
        let min_child_size = if self.must_fill { bc.min() } else { Size::ZERO };
        let child_bc =
            BoxConstraints::new(min_child_size, Size::new(max_child_width, max_child_height));

        // println!("child bc: {:?}", child_bc);

        let bc_changed = child_bc != self.old_bc;
        self.old_bc = child_bc;

        let content_size = if bc_changed || self.container.layout_requested() {
            self.container.layout(ctx, &child_bc, data, env)
            // self.container.layout_rect().size()
        } else {
            self.container.layout_rect().size()
        };

        self.port.content_size = content_size;
        self.port.view_size = bc.constrain(content_size);
        self.port.sanitize_view_origin();

        // TODO: this might be the place where we can defer the update
        //
        self.container
            .set_origin(ctx, (Point::ZERO - self.port.view_origin).to_point());

        if self.viewport_size() != self.old_size {
            ctx.view_context_changed();
            self.old_size = self.viewport_size();
        }
        println!("container rect: {:?}", self.container.layout_rect());

        trace!("Computed sized: {}", self.viewport_size());
        self.viewport_size()
    }

    #[instrument(name = "ClipBox", level = "trace", skip(self, ctx, data, env))]
    fn paint(&mut self, ctx: &mut PaintCtx, data: &T, env: &Env) {
        let clip_rect = ctx.size().to_rect();
        ctx.clip(clip_rect);
        self.container.paint(ctx, data, env);
    }

    fn debug_state(&self, data: &T) -> DebugState {
        DebugState {
            display_name: self.short_type_name().to_string(),
            children: vec![self.container.widget().debug_state(data)],
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::Viewport;
    use super::*;
    use test_log::test;

    #[test]
    fn pan_to_visible() {
        let mut viewport = Viewport {
            content_size: Size::new(400., 400.),
            view_size: (20., 20.).into(),
            view_origin: (20., 20.).into(),
        };

        assert!(!viewport.pan_to_visible(Rect::from_origin_size((22., 22.,), (5., 5.))));
        assert!(viewport.pan_to_visible(Rect::from_origin_size((10., 10.,), (5., 5.))));
        assert_eq!(viewport.view_origin, Point::new(10., 10.));
        assert_eq!(viewport.view_size, Size::new(20., 20.));
        assert!(!viewport.pan_to_visible(Rect::from_origin_size((10., 10.,), (50., 50.))));
        assert_eq!(viewport.view_origin, Point::new(10., 10.));

        assert!(viewport.pan_to_visible(Rect::from_origin_size((30., 10.,), (5., 5.))));
        assert_eq!(viewport.view_origin, Point::new(15., 10.));
        assert!(viewport.pan_to_visible(Rect::from_origin_size((5., 5.,), (5., 5.))));
        assert_eq!(viewport.view_origin, Point::new(5., 5.));
    }
}

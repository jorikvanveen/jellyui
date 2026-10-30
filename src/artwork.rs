//! Decoded artwork belongs to a visible element, never GPUI's global asset cache.
use std::sync::Arc;

use gpui::{
    AnyElement, App, AppContext, Bounds, Context, Element, ElementId, Entity, GlobalElementId,
    Image, ImageCacheError, ImageStyle, InspectorElementId, IntoElement, LayoutId, Pixels,
    RenderImage, StyleRefinement, Styled, StyledImage, Task, Window, img,
};

pub fn image(source: Arc<Image>) -> VisibleImage {
    VisibleImage {
        source,
        style: StyleRefinement::default(),
        image_style: ImageStyle::default(),
    }
}

pub struct VisibleImage {
    source: Arc<Image>,
    style: StyleRefinement,
    image_style: ImageStyle,
}

impl Styled for VisibleImage {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl StyledImage for VisibleImage {
    fn image_style(&mut self) -> &mut ImageStyle {
        &mut self.image_style
    }
}

#[derive(Default)]
pub struct ResidentImage {
    visible: bool,
    generation: u64,
    data: Option<Result<Arc<RenderImage>, ImageCacheError>>,
    task: Option<Task<()>>,
}

impl ResidentImage {
    fn release(&mut self) -> Option<Arc<RenderImage>> {
        self.visible = false;
        self.generation = self.generation.wrapping_add(1);
        self.task = None;
        self.data.take().and_then(Result::ok)
    }

    fn clear(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(image) = self.release() {
            tracing::debug!(image = ?image.id, "Releasing offscreen artwork");
            cx.drop_image(image, Some(window));
        }
    }

    fn finish(
        &mut self,
        generation: u64,
        result: Result<Arc<RenderImage>, ImageCacheError>,
    ) -> bool {
        if !self.visible || self.generation != generation {
            return false;
        }
        self.task = None;
        self.data = Some(result);
        true
    }

    fn set_visible(
        &mut self,
        visible: bool,
        source: Arc<Image>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if visible == self.visible {
            return;
        }
        if !visible {
            self.clear(window, cx);
            return;
        }
        self.visible = true;
        let generation = self.generation;
        let owner = window.current_view();
        let renderer = cx.svg_renderer();
        let decode = cx.background_executor().spawn(async move {
            source
                .to_image_data(renderer)
                .map_err(ImageCacheError::from)
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = decode.await;
            let _ = this.update(cx, |image, cx| {
                if image.finish(generation, result) {
                    match image.data.as_ref() {
                        Some(Ok(data)) => tracing::debug!(image = ?data.id, size = ?data.size(0), "Visible artwork decoded"),
                        Some(Err(error)) => tracing::warn!(%error, "Could not decode artwork"),
                        None => {}
                    }
                    App::notify(cx, owner);
                }
            });
        }));
    }
}

pub struct ImageLayout {
    inner: AnyElement,
    resident: Entity<ResidentImage>,
}

impl Element for VisibleImage {
    type RequestLayoutState = ImageLayout;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(ElementId::Integer(self.source.id()))
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ImageLayout) {
        window.with_element_state(
            id.expect("artwork has an element ID"),
            |state: Option<Entity<ResidentImage>>, window| {
                let resident = state.unwrap_or_else(|| {
                    cx.new(|cx| {
                        cx.on_release_in(window, |image: &mut ResidentImage, window, cx| {
                            image.clear(window, cx)
                        })
                        .detach();
                        ResidentImage::default()
                    })
                });
                let source = resident.clone();
                let mut inner =
                    img(move |_: &mut Window, cx: &mut App| source.read(cx).data.clone());
                *inner.style() = self.style.clone();
                *inner.image_style() = std::mem::take(&mut self.image_style);
                let mut inner = inner.into_any_element();
                let layout = inner.request_layout(window, cx);
                (
                    (
                        layout,
                        ImageLayout {
                            inner,
                            resident: resident.clone(),
                        },
                    ),
                    resident,
                )
            },
        )
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut ImageLayout,
        window: &mut Window,
        cx: &mut App,
    ) {
        let visible = intersects_viewport(bounds, window.content_mask().bounds);
        state.resident.update(cx, |image, cx| {
            image.set_visible(visible, self.source.clone(), window, cx)
        });
        state.inner.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        state: &mut ImageLayout,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        state.inner.paint(window, cx);
    }
}

impl IntoElement for VisibleImage {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

fn intersects_viewport(bounds: Bounds<Pixels>, viewport: Bounds<Pixels>) -> bool {
    let intersection = bounds.intersect(&viewport);
    intersection.size.width > gpui::px(0.) && intersection.size.height > gpui::px(0.)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    fn decoded_image() -> Arc<RenderImage> {
        Arc::new(RenderImage::new([image::Frame::new(
            image::RgbaImage::new(2, 2),
        )]))
    }

    #[test]
    fn releasing_an_image_drops_its_pixels_and_invalidates_pending_work() {
        let decoded = decoded_image();
        let pixels = Arc::downgrade(&decoded);
        let mut resident = ResidentImage {
            visible: true,
            data: Some(Ok(decoded)),
            task: Some(Task::ready(())),
            ..ResidentImage::default()
        };
        let generation = resident.generation;
        let texture_to_release = resident.release().unwrap();
        assert!(!resident.visible);
        assert!(resident.data.is_none());
        assert!(resident.task.is_none());
        assert_ne!(resident.generation, generation);
        drop(texture_to_release);
        assert!(pixels.upgrade().is_none());
    }

    #[test]
    fn late_decodes_cannot_repopulate_an_offscreen_or_reused_element() {
        let mut resident = ResidentImage {
            visible: true,
            ..ResidentImage::default()
        };
        let old_generation = resident.generation;
        resident.release();
        assert!(!resident.finish(old_generation, Ok(decoded_image())));
        resident.visible = true;
        resident.task = Some(Task::ready(()));
        assert!(!resident.finish(old_generation, Ok(decoded_image())));
        assert!(resident.task.is_some());
        assert!(resident.data.is_none());
        assert!(resident.finish(resident.generation, Ok(decoded_image())));
        assert!(resident.data.as_ref().unwrap().is_ok());
        assert!(resident.task.is_none());
    }

    #[test]
    fn only_artwork_with_visible_pixels_is_resident() {
        let viewport = Bounds::new(point(px(10.), px(100.)), size(px(300.), px(400.)));
        let bounds = |x, y, w, h| Bounds::new(point(px(x), px(y)), size(px(w), px(h)));
        assert!(intersects_viewport(bounds(20., 110., 100., 100.), viewport));
        assert!(intersects_viewport(bounds(20., 490., 100., 100.), viewport));
        assert!(!intersects_viewport(
            bounds(20., 500., 100., 100.),
            viewport
        ));
        assert!(!intersects_viewport(
            bounds(20., -10., 100., 100.),
            viewport
        ));
        assert!(!intersects_viewport(
            bounds(310., 110., 100., 100.),
            viewport
        ));
        assert!(!intersects_viewport(bounds(20., 110., 0., 100.), viewport));
    }
}

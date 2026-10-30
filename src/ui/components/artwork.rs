//! Decoded artwork belongs to a visible element, never GPUI's global asset cache.
mod raster;

use raster::{Fit, Target, downsample};
use std::{sync::Arc, time::Duration};

use gpui::{
    AnyElement, App, AppContext, Bounds, Context, Element, ElementId, Entity, GlobalElementId,
    Image, ImageCacheError, ImageStyle, InspectorElementId, IntoElement, LayoutId, ObjectFit,
    Pixels, RenderImage, StyleRefinement, Styled, StyledImage, Task, Window, img,
};

pub fn image(source: Arc<Image>) -> VisibleImage {
    VisibleImage {
        source,
        resize: None,
        style: StyleRefinement::default(),
        image_style: ImageStyle::default(),
    }
}

pub struct VisibleImage {
    source: Arc<Image>,
    resize: Option<Fit>,
    style: StyleRefinement,
    image_style: ImageStyle,
}

impl VisibleImage {
    /// Keep a raster sized for this element's display, preserving its fit policy.
    pub fn resize_to_display(mut self, fit: ObjectFit) -> Self {
        self.resize = Fit::from_object_fit(&fit);
        self.object_fit(fit)
    }
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
    target: Option<Target>,
}

impl ResidentImage {
    fn release(&mut self) -> Option<Arc<RenderImage>> {
        self.visible = false;
        self.target = None;
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
        target: Option<Target>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if visible == self.visible && (!visible || self.target == target) {
            return;
        }
        if !visible {
            self.clear(window, cx);
            return;
        }
        let debounce = self.visible;
        self.task = None;
        self.visible = true;
        self.target = target;
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let owner = window.current_view();
        let renderer = cx.svg_renderer();
        self.task = Some(cx.spawn(async move |this, cx| {
            // Keep the current raster while a window resize is in progress.
            // Repeated size changes cancel this timer before CPU work starts.
            if debounce {
                cx.background_executor().timer(Duration::from_millis(100)).await;
            }
            let decode = cx.background_executor().spawn(async move {
                source.to_image_data(renderer)
                    .map(|data| match target { Some(target) => downsample(data, target), None => data })
                    .map_err(ImageCacheError::from)
            });
            let result = decode.await;
            let _ = this.update(cx, |image, cx| {
                let previous = image.data.as_ref().and_then(|data| data.as_ref().ok()).cloned();
                if image.finish(generation, result) {
                    if let Some(previous) = previous {
                        cx.drop_image(previous, None);
                    }
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
        let target = self.resize.map(|fit| Target {
            width: (f32::from(bounds.size.width) * window.scale_factor())
                .ceil()
                .max(1.) as u32,
            height: (f32::from(bounds.size.height) * window.scale_factor())
                .ceil()
                .max(1.) as u32,
            fit,
        });
        state.resident.update(cx, |image, cx| {
            image.set_visible(visible, self.source.clone(), target, window, cx)
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

    fn decoded_image() -> Arc<RenderImage> {
        Arc::new(RenderImage::new([image::Frame::new(
            image::RgbaImage::new(2, 2),
        )]))
    }

    #[test]
    fn releasing_an_image_drops_its_pixels() {
        let decoded = decoded_image();
        let pixels = Arc::downgrade(&decoded);
        let mut resident = ResidentImage {
            visible: true,
            data: Some(Ok(decoded)),
            task: Some(Task::ready(())),
            ..ResidentImage::default()
        };
        let texture_to_release = resident.release().unwrap();
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
        assert!(!resident.finish(old_generation, Ok(decoded_image())));
        assert!(resident.data.is_none());
        assert!(resident.finish(resident.generation, Ok(decoded_image())));
        assert!(resident.data.as_ref().unwrap().is_ok());
    }
}

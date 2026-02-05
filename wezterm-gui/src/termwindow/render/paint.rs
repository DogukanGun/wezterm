use crate::termwindow::box_model::{
    Element, ElementColors, ElementContent, LayoutContext,
};
use crate::termwindow::{RenderFrame, TermWindowNotif};
use ::window::bitmaps::atlas::OutOfTextureSpace;
use ::window::WindowOps;
use anyhow::Context;
use config::DimensionContext;
use mux::tab::PositionedPane;
use smol::Timer;
use std::time::{Duration, Instant};
use wezterm_font::ClearShapeCache;
use euclid::rect;
use window::color::LinearRgba;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowImage {
    Yes,
    Scale(usize),
    No,
}

impl crate::TermWindow {
    pub fn paint_impl(&mut self, frame: &mut RenderFrame) {
        self.num_frames += 1;
        // If nothing on screen needs animating, then we can avoid
        // invalidating as frequently
        *self.has_animation.borrow_mut() = None;
        // Start with the assumption that we should allow images to render
        self.allow_images = AllowImage::Yes;

        let start = Instant::now();

        {
            let diff = start.duration_since(self.last_fps_check_time);
            if diff > Duration::from_secs(1) {
                let seconds = diff.as_secs_f32();
                self.fps = self.num_frames as f32 / seconds;
                self.num_frames = 0;
                self.last_fps_check_time = start;
            }
        }

        'pass: for pass in 0.. {
            match self.paint_pass() {
                Ok(_) => match self.render_state.as_mut().unwrap().allocated_more_quads() {
                    Ok(allocated) => {
                        if !allocated {
                            break 'pass;
                        }
                        self.invalidate_fancy_tab_bar();
                        self.invalidate_modal();
                    }
                    Err(err) => {
                        log::error!("{:#}", err);
                        break 'pass;
                    }
                },
                Err(err) => {
                    if let Some(&OutOfTextureSpace {
                        size: Some(size),
                        current_size,
                    }) = err.root_cause().downcast_ref::<OutOfTextureSpace>()
                    {
                        let result = if pass == 0 {
                            // Let's try clearing out the atlas and trying again
                            // self.clear_texture_atlas()
                            log::trace!("recreate_texture_atlas");
                            self.recreate_texture_atlas(Some(current_size))
                        } else {
                            log::trace!("grow texture atlas to {}", size);
                            self.recreate_texture_atlas(Some(size))
                        };
                        self.invalidate_fancy_tab_bar();
                        self.invalidate_modal();

                        if let Err(err) = result {
                            self.allow_images = match self.allow_images {
                                AllowImage::Yes => AllowImage::Scale(2),
                                AllowImage::Scale(2) => AllowImage::Scale(4),
                                AllowImage::Scale(4) => AllowImage::Scale(8),
                                AllowImage::Scale(8) => AllowImage::No,
                                AllowImage::No | _ => {
                                    log::error!(
                                        "Failed to {} texture: {}",
                                        if pass == 0 { "clear" } else { "resize" },
                                        err
                                    );
                                    break 'pass;
                                }
                            };

                            log::info!(
                                "Not enough texture space ({:#}); \
                                     will retry render with {:?}",
                                err,
                                self.allow_images,
                            );
                        }
                    } else if err.root_cause().downcast_ref::<ClearShapeCache>().is_some() {
                        self.invalidate_fancy_tab_bar();
                        self.invalidate_modal();
                        self.shape_generation += 1;
                        self.shape_cache.borrow_mut().clear();
                        self.line_to_ele_shape_cache.borrow_mut().clear();
                    } else {
                        log::error!("paint_pass failed: {:#}", err);
                        break 'pass;
                    }
                }
            }
        }
        log::debug!("paint_impl before call_draw elapsed={:?}", start.elapsed());

        self.call_draw(frame).ok();
        self.last_frame_duration = start.elapsed();
        log::debug!(
            "paint_impl elapsed={:?}, fps={}",
            self.last_frame_duration,
            self.fps
        );
        metrics::histogram!("gui.paint.impl").record(self.last_frame_duration);
        metrics::histogram!("gui.paint.impl.rate").record(1.);

        // If self.has_animation is some, then the last render detected
        // image attachments with multiple frames, so we also need to
        // invalidate the viewport when the next frame is due
        if self.focused.is_some() {
            if let Some(next_due) = *self.has_animation.borrow() {
                let prior = self.scheduled_animation.borrow_mut().take();
                match prior {
                    Some(prior) if prior <= next_due => {
                        // Already due before that time
                    }
                    _ => {
                        self.scheduled_animation.borrow_mut().replace(next_due);
                        let window = self.window.clone().take().unwrap();
                        promise::spawn::spawn(async move {
                            Timer::at(next_due).await;
                            let win = window.clone();
                            window.notify(TermWindowNotif::Apply(Box::new(move |tw| {
                                tw.scheduled_animation.borrow_mut().take();
                                win.invalidate();
                            })));
                        })
                        .detach();
                    }
                }
            }
        }
    }

    /// Row Y and geometry for the AI inline line at the pane's cursor (so you type where the prompt is).
    fn ai_inline_line_geometry(
        &self,
        pos: &PositionedPane,
    ) -> (f32, f32, f32, f32) {
        let (padding_left, padding_top) = self.padding_left_top();
        let tab_bar_height = self.tab_bar_pixel_height().unwrap_or(0.);
        let (top_bar_height, _bottom_bar_height) = if self.config.tab_bar_at_bottom {
            (0.0, tab_bar_height)
        } else {
            (tab_bar_height, 0.0)
        };
        let border = self.get_os_border();
        let top_pixel_y = top_bar_height + padding_top + border.top.get() as f32;
        let cell_width = self.render_metrics.cell_size.width as f32;
        let cell_height = self.render_metrics.cell_size.height as f32;

        let dims = pos.pane.get_dimensions();
        let viewport_top = self
            .get_viewport(pos.pane.pane_id())
            .unwrap_or(dims.physical_top);
        let cursor = pos.pane.get_cursor_position();
        let line_idx = (cursor.y - viewport_top)
            .max(0)
            .min((dims.viewport_rows as isize).saturating_sub(1)) as usize;
        let row_y = top_pixel_y + (pos.top + line_idx) as f32 * cell_height;
        let line_left = padding_left + border.left.get() as f32 + pos.left as f32 * cell_width;
        let line_width = pos.width as f32 * cell_width;
        (row_y, line_left, line_width, cell_height)
    }

    /// Draw only the background rect for the AI inline line (uses existing layers borrow).
    fn paint_ai_inline_line_background(
        &mut self,
        layers: &mut crate::quad::TripleLayerQuadAllocator,
        pos: &PositionedPane,
    ) -> anyhow::Result<()> {
        let (row_y, line_left, line_width, cell_height) = self.ai_inline_line_geometry(pos);
        let palette = pos.pane.palette();
        let bg = palette.background.to_linear();

        self.filled_rectangle(
            layers,
            0,
            rect(line_left, row_y, line_width, cell_height),
            bg,
        )
        .context("ai inline bg rect")?;
        Ok(())
    }

    /// Draw the AI inline prompt + buffer text. Called after layers borrow is released
    /// so that render_element can take its own quad_allocator.
    fn paint_ai_inline_line_text(&mut self) -> anyhow::Result<()> {
        use crate::termwindow::box_model::BorderColor;

        let active_pos = match self.get_panes_to_render().into_iter().find(|p| p.is_active) {
            Some(p) => p,
            None => return Ok(()),
        };
        let pos = &active_pos;

        let (row_y, line_left, line_width, cell_height) = self.ai_inline_line_geometry(pos);

        let palette = pos.pane.palette();
        let fg = palette.foreground.to_linear();

        let buffer = self.ai_inline_buffer.borrow().clone();
        let mut line_text = String::new();

        // Prefer the snapshot captured when we entered AI mode so that the
        // prompt stays visually identical across interactions.
        let prompt = self.ai_inline_prompt.borrow().clone();
        if !prompt.is_empty() {
            line_text.push_str(&prompt);
        } else {
            // Fallback: derive prompt from the current cursor line if we don't
            // have a snapshot (e.g. AI mode was enabled before this field existed).
            let cursor = pos.pane.get_cursor_position();
            let (_first_row, line_vec) = pos.pane.get_lines(cursor.y..cursor.y + 1);
            if let Some(line) = line_vec.first() {
                line_text.push_str(&line.columns_as_str(0..cursor.x));
            }
        }
        line_text.push_str(&buffer);
        let font_style = self
            .config
            .command_palette_font
            .as_ref()
            .unwrap_or(&self.config.font);
        let font = self.fonts.resolve_font(font_style).context("resolve font")?;
        let metrics = crate::utilsprites::RenderMetrics::with_font_metrics(&font.metrics());

        let bounds = rect(line_left, row_y, line_width, cell_height);
        let layout_ctx = LayoutContext {
            height: DimensionContext {
                dpi: self.dimensions.dpi as f32,
                pixel_max: self.dimensions.pixel_height as f32,
                pixel_cell: metrics.cell_size.height as f32,
            },
            width: DimensionContext {
                dpi: self.dimensions.dpi as f32,
                pixel_max: self.dimensions.pixel_width as f32,
                pixel_cell: metrics.cell_size.width as f32,
            },
            bounds,
            metrics: &metrics,
            gl_state: self.render_state.as_ref().unwrap(),
            zindex: 0,
        };
        let element = Element::new(&font, ElementContent::Text(line_text)).colors(ElementColors {
            border: BorderColor::default(),
            bg: LinearRgba::TRANSPARENT.into(),
            text: fg.into(),
        });
        let computed = self.compute_element(&layout_ctx, &element)?;
        let gl_state = self.render_state.as_ref().unwrap();
        self.render_element(&computed, gl_state, None)?;
        Ok(())
    }

    pub fn paint_modal(&mut self) -> anyhow::Result<()> {
        if let Some(modal) = self.get_modal() {
            for computed in modal.computed_element(self)?.iter() {
                let mut ui_items = computed.ui_items();

                let gl_state = self.render_state.as_ref().unwrap();
                self.render_element(&computed, gl_state, None)?;

                self.ui_items.append(&mut ui_items);
            }
        }

        Ok(())
    }

    pub fn paint_pass(&mut self) -> anyhow::Result<()> {
        {
            let gl_state = self.render_state.as_ref().unwrap();
            for layer in gl_state.layers.borrow().iter() {
                layer.clear_quad_allocation();
            }
        }

        // Clear out UI item positions; we'll rebuild these as we render
        self.ui_items.clear();

        let panes = self.get_panes_to_render();
        let focused = self.focused.is_some();
        let window_is_transparent =
            !self.window_background.is_empty() || self.config.window_background_opacity != 1.0;

        let start = Instant::now();
        let gl_state = self.render_state.as_ref().unwrap();
        let layer = gl_state
            .layer_for_zindex(0)
            .context("layer_for_zindex(0)")?;
        let mut layers = layer.quad_allocator();
        log::trace!("quad map elapsed {:?}", start.elapsed());
        metrics::histogram!("quad.map").record(start.elapsed());

        let mut paint_terminal_background = false;

        // Render the full window background
        match (self.window_background.is_empty(), self.allow_images) {
            (false, AllowImage::Yes | AllowImage::Scale(_)) => {
                let bg_color = self.palette().background.to_linear();

                let top = panes
                    .iter()
                    .find(|p| p.is_active)
                    .map(|p| match self.get_viewport(p.pane.pane_id()) {
                        Some(top) => top,
                        None => p.pane.get_dimensions().physical_top,
                    })
                    .unwrap_or(0);

                let loaded_any = self
                    .render_backgrounds(bg_color, top)
                    .context("render_backgrounds")?;

                if !loaded_any {
                    // Either there was a problem loading the background(s)
                    // or they haven't finished loading yet.
                    // Use the regular terminal background until that changes.
                    paint_terminal_background = true;
                }
            }
            _ if window_is_transparent => {
                // Avoid doubling up the background color: the panes
                // will render out through the padding so there
                // should be no gaps that need filling in
            }
            _ => {
                paint_terminal_background = true;
            }
        }

        if paint_terminal_background {
            // Regular window background color
            let background = if panes.len() == 1 {
                // If we're the only pane, use the pane's palette
                // to draw the padding background
                panes[0].pane.palette().background
            } else {
                self.palette().background
            }
            .to_linear()
            .mul_alpha(self.config.window_background_opacity);

            self.filled_rectangle(
                &mut layers,
                0,
                euclid::rect(
                    0.,
                    0.,
                    self.dimensions.pixel_width as f32,
                    self.dimensions.pixel_height as f32,
                ),
                background,
            )
            .context("filled_rectangle for window background")?;
        }

        for pos in panes {
            if pos.is_active {
                self.update_text_cursor(&pos);
                if focused {
                    pos.pane.advise_focus();
                    mux::Mux::get().record_focus_for_current_identity(pos.pane.pane_id());
                }
            }
            self.paint_pane(&pos, &mut layers).context("paint_pane")?;
        }

        if self.current_mode == crate::termwindow::PaneMode::Ai {
            if let Some(active_pos) = self.get_panes_to_render().into_iter().find(|p| p.is_active) {
                self.paint_ai_inline_line_background(&mut layers, &active_pos)
                    .context("paint_ai_inline_line_background")?;
            }
        }

        if let Some(pane) = self.get_active_pane_or_overlay() {
            let splits = self.get_splits();
            for split in &splits {
                self.paint_split(&mut layers, split, &pane)
                    .context("paint_split")?;
            }
        }

        if self.show_tab_bar {
            self.paint_tab_bar(&mut layers).context("paint_tab_bar")?;
        }

        self.paint_window_borders(&mut layers)
            .context("paint_window_borders")?;
        drop(layers);

        if self.current_mode == crate::termwindow::PaneMode::Ai {
            self.paint_ai_inline_line_text()
                .context("paint_ai_inline_line_text")?;
        }
        self.paint_modal().context("paint_modal")?;

        Ok(())
    }
}

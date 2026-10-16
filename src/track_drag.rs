use eframe::egui::{self, Color32, CursorIcon, DragAndDrop, Id, Pos2, Rect, Sense, Stroke, Vec2};
use loudtrace::project::Project;
use uuid::Uuid;

use crate::ui_style::{ACCENT, ACCENT_INK, INK, RAISED, T_BODY_MEDIUM, layout_text};

#[derive(Clone, Copy)]
pub struct TrackDrag {
    pub project_id: Uuid,
    pub track_id: Uuid,
}

pub fn is_dragging(ctx: &egui::Context, project_id: Uuid, track_id: Uuid) -> bool {
    DragAndDrop::payload::<TrackDrag>(ctx)
        .is_some_and(|drag| drag.project_id == project_id && drag.track_id == track_id)
}

pub fn source(
    ui: &egui::Ui,
    rect: Rect,
    id: Id,
    payload: TrackDrag,
    enabled: bool,
) -> egui::Response {
    let response = ui.interact(
        rect,
        id,
        if enabled {
            Sense::click_and_drag()
        } else {
            Sense::hover()
        },
    );
    if enabled {
        // A click remains inert; only crossing the drag threshold starts a reorder.
        if response.drag_started_by(egui::PointerButton::Primary) {
            DragAndDrop::set_payload(ui.ctx(), payload);
        }
        response
            .on_hover_cursor(CursorIcon::Grab)
            .on_hover_text("Drag to reorder")
    } else {
        response
    }
}

fn insertion_slot(
    pointer: Pos2,
    viewport: Rect,
    content_top: f32,
    row_height: f32,
    count: usize,
) -> Option<usize> {
    (viewport.width() > 0.0 && viewport.height() > 2.0 && viewport.contains(pointer)).then(|| {
        ((pointer.y - content_top) / row_height + 0.5)
            .floor()
            .clamp(0.0, count as f32) as usize
    })
}

fn scroll_speed(pointer: Pos2, viewport: Rect) -> f32 {
    if !viewport.contains(pointer) {
        return 0.0;
    }
    let edge = 32.0_f32.min(viewport.height() / 2.0);
    if edge <= 0.0 {
        return 0.0;
    }
    // Positive values move the content downward, toward earlier tracks.
    let top = (1.0 - (pointer.y - viewport.top()) / edge).clamp(0.0, 1.0);
    let bottom = (1.0 - (viewport.bottom() - pointer.y) / edge).clamp(0.0, 1.0);
    (top - bottom) * 480.0
}

pub fn auto_scroll(ui: &egui::Ui, project_id: Uuid, viewport: Rect, enabled: bool) {
    if !enabled
        || !DragAndDrop::payload::<TrackDrag>(ui.ctx())
            .is_some_and(|drag| drag.project_id == project_id)
    {
        return;
    }
    let (pointer, held, dt) = ui.input(|i| {
        (
            i.pointer.interact_pos(),
            i.pointer.primary_down(),
            i.stable_dt,
        )
    });
    if held && let Some(pointer) = pointer {
        let delta = scroll_speed(pointer, viewport) * dt.min(0.05);
        if delta != 0.0 {
            ui.scroll_with_delta_animation(
                Vec2::new(0.0, delta),
                egui::style::ScrollAnimation::none(),
            );
            ui.ctx().request_repaint();
        }
    }
}

pub fn drop_target(
    ui: &egui::Ui,
    project: &Project,
    viewport: Rect,
    content_top: f32,
    row_height: f32,
    enabled: bool,
) -> Option<(Uuid, Option<Uuid>)> {
    let drag = DragAndDrop::payload::<TrackDrag>(ui.ctx())?;
    let source_index = project
        .tracks
        .iter()
        .position(|track| track.id == drag.track_id);
    if !enabled
        || drag.project_id != project.id
        || source_index.is_none()
        || !ui.input(|i| i.focused)
    {
        DragAndDrop::clear_payload(ui.ctx());
        return None;
    }
    let pointer = ui.input(|i| i.pointer.interact_pos())?;
    let slot = insertion_slot(
        pointer,
        viewport,
        content_top,
        row_height,
        project.tracks.len(),
    );
    ui.ctx().set_cursor_icon(if slot.is_some() {
        CursorIcon::Grabbing
    } else {
        CursorIcon::NotAllowed
    });
    let slot = slot?;
    let source_index = source_index?;
    let changes_order = slot != source_index && slot != source_index + 1;
    let painter = ui
        .painter()
        .with_clip_rect(viewport.intersect(ui.clip_rect()));
    if changes_order {
        let y = (content_top + slot as f32 * row_height)
            .clamp(viewport.top() + 1.0, viewport.bottom() - 1.0);
        let left = viewport.left() + 16.0;
        let right = viewport.right() - 16.0;
        painter.line_segment(
            [egui::pos2(left, y), egui::pos2(right, y)],
            Stroke::new(2.0, ACCENT),
        );
        painter.circle_filled(egui::pos2(left, y), 3.0, ACCENT);
    }

    if ui.input(|i| i.pointer.button_released(egui::PointerButton::Primary)) {
        // Commit exactly once on release; Escape and outside drops are discarded by egui.
        DragAndDrop::take_payload::<TrackDrag>(ui.ctx());
        return changes_order.then(|| {
            (
                drag.track_id,
                project.tracks.get(slot).map(|track| track.id),
            )
        });
    }

    let name = project.tracks[source_index].display_name();
    let galley = layout_text(
        ui,
        &name,
        T_BODY_MEDIUM,
        INK,
        232.0_f32.min(viewport.width() - 24.0),
    );
    let size = galley.size() + Vec2::new(24.0, 16.0);
    let position = egui::pos2(
        (pointer.x + 14.0).min(viewport.right() - size.x),
        (pointer.y + 16.0).min(viewport.bottom() - size.y),
    );
    let preview = Rect::from_min_size(position, size);
    painter.rect_filled(
        preview.translate(Vec2::new(0.0, 3.0)),
        6.0,
        Color32::from_black_alpha(65),
    );
    painter.rect_filled(preview, 6.0, RAISED);
    painter.rect_stroke(
        preview,
        6.0,
        Stroke::new(1.0, ACCENT_INK),
        egui::StrokeKind::Inside,
    );
    painter.galley(position + Vec2::new(12.0, 8.0), galley, INK);
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use loudtrace::project::Track;

    const ROW_HEIGHT: f32 = 40.0;
    const CONTENT_TOP: f32 = 40.0;

    #[derive(Default)]
    struct FrameResult {
        clicked: bool,
        started: bool,
        dropped: Option<(Uuid, Option<Uuid>)>,
        duplicate_drop: Option<(Uuid, Option<Uuid>)>,
    }

    fn project(count: usize) -> Project {
        let mut project = Project::new("Drag regression".into());
        project.tracks = (0..count)
            .map(|index| Track::new(format!("Track {index}.wav").into()))
            .collect();
        project
    }

    fn viewport() -> Rect {
        Rect::from_min_max(egui::pos2(20.0, 40.0), egui::pos2(380.0, 280.0))
    }

    fn button(pos: Pos2, button: egui::PointerButton, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn primary(pos: Pos2, pressed: bool) -> egui::Event {
        button(pos, egui::PointerButton::Primary, pressed)
    }

    fn frame(
        ctx: &egui::Context,
        project: &Project,
        events: Vec<egui::Event>,
        content_top: f32,
        enabled: bool,
        focused: bool,
    ) -> FrameResult {
        let mut result = FrameResult::default();
        ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 320.0))),
                events,
                focused,
                ..Default::default()
            },
            |ui| {
                let response = source(
                    ui,
                    Rect::from_min_max(
                        egui::pos2(40.0, content_top + ROW_HEIGHT),
                        egui::pos2(360.0, content_top + 2.0 * ROW_HEIGHT),
                    ),
                    Id::new("drag-regression-source"),
                    TrackDrag {
                        project_id: project.id,
                        track_id: project.tracks[1].id,
                    },
                    enabled,
                );
                result.clicked = response.clicked();
                result.started = response.drag_started();
                result.dropped =
                    drop_target(ui, project, viewport(), content_top, ROW_HEIGHT, enabled);
                result.duplicate_drop =
                    drop_target(ui, project, viewport(), content_top, ROW_HEIGHT, enabled);
            },
        )
        .drop_without_applying_deltas();
        result
    }

    fn start_drag(ctx: &egui::Context, project: &Project, target: Pos2) {
        frame(ctx, project, vec![], CONTENT_TOP, true, true);
        let start = egui::pos2(100.0, 100.0);
        frame(
            ctx,
            project,
            vec![egui::Event::PointerMoved(start), primary(start, true)],
            CONTENT_TOP,
            true,
            true,
        );
        let moved = frame(
            ctx,
            project,
            vec![egui::Event::PointerMoved(target)],
            CONTENT_TOP,
            true,
            true,
        );
        assert!(moved.started);
        assert!(moved.dropped.is_none());
        assert!(is_dragging(ctx, project.id, project.tracks[1].id));
    }

    #[test]
    fn clicks_and_subthreshold_movement_do_not_start_reordering() {
        let ctx = egui::Context::default();
        crate::ui_style::configure_style(&ctx).expect("system fonts");
        let project = project(6);
        frame(&ctx, &project, vec![], CONTENT_TOP, true, true);
        let start = egui::pos2(100.0, 100.0);
        let nearby = start + Vec2::new(1.0, 1.0);
        let pressed = frame(
            &ctx,
            &project,
            vec![egui::Event::PointerMoved(start), primary(start, true)],
            CONTENT_TOP,
            true,
            true,
        );
        assert!(!pressed.started);
        let moved = frame(
            &ctx,
            &project,
            vec![egui::Event::PointerMoved(nearby)],
            CONTENT_TOP,
            true,
            true,
        );
        assert!(!moved.started);
        assert!(!DragAndDrop::has_payload_of_type::<TrackDrag>(&ctx));
        let released = frame(
            &ctx,
            &project,
            vec![primary(nearby, false)],
            CONTENT_TOP,
            true,
            true,
        );
        assert!(released.clicked);
        assert!(released.dropped.is_none());
        assert!(!DragAndDrop::has_payload_of_type::<TrackDrag>(&ctx));
    }

    #[test]
    fn drag_commits_once_on_primary_release() {
        let ctx = egui::Context::default();
        crate::ui_style::configure_style(&ctx).expect("system fonts");
        let project = project(6);
        let target = egui::pos2(150.0, 205.0);
        start_drag(&ctx, &project, target);
        assert!(
            frame(&ctx, &project, vec![], CONTENT_TOP, true, true)
                .dropped
                .is_none()
        );
        let released = frame(
            &ctx,
            &project,
            vec![primary(target, false)],
            CONTENT_TOP,
            true,
            true,
        );
        assert_eq!(
            released.dropped,
            Some((project.tracks[1].id, Some(project.tracks[4].id)))
        );
        assert!(released.duplicate_drop.is_none());
        assert!(!DragAndDrop::has_payload_of_type::<TrackDrag>(&ctx));
        assert!(
            frame(&ctx, &project, vec![], CONTENT_TOP, true, true)
                .dropped
                .is_none()
        );
    }

    #[test]
    fn release_maps_before_after_and_append_slots_in_virtualized_content() {
        for (content_top, count, y, expected_slot) in [
            (40.0, 6, 41.0, Some(0)),
            (40.0, 6, 205.0, Some(4)),
            (40.0, 6, 235.0, Some(5)),
            (40.0, 6, 279.0, None),
            (-120.0, 10, 41.0, Some(4)),
            (-120.0, 10, 59.0, Some(4)),
            (-120.0, 10, 61.0, Some(5)),
            (-120.0, 10, 100.0, Some(6)),
            (-120.0, 10, 279.0, None),
        ] {
            let ctx = egui::Context::default();
            crate::ui_style::configure_style(&ctx).expect("system fonts");
            let project = project(count);
            let pointer = egui::pos2(150.0, y);
            frame(&ctx, &project, vec![], content_top, true, true);
            DragAndDrop::set_payload(
                &ctx,
                TrackDrag {
                    project_id: project.id,
                    track_id: project.tracks[1].id,
                },
            );
            frame(
                &ctx,
                &project,
                vec![primary(pointer, true)],
                content_top,
                true,
                true,
            );
            let released = frame(
                &ctx,
                &project,
                vec![primary(pointer, false)],
                content_top,
                true,
                true,
            );
            assert_eq!(
                released.dropped,
                Some((
                    project.tracks[1].id,
                    expected_slot.map(|index| project.tracks[index].id)
                )),
                "content_top={content_top}, pointer_y={y}",
            );
        }
    }

    #[test]
    fn dropping_on_either_half_of_the_source_row_is_a_no_op() {
        for y in [85.0, 115.0] {
            let ctx = egui::Context::default();
            crate::ui_style::configure_style(&ctx).expect("system fonts");
            let project = project(6);
            start_drag(&ctx, &project, egui::pos2(180.0, 200.0));
            let target = egui::pos2(180.0, y);
            let released = frame(
                &ctx,
                &project,
                vec![egui::Event::PointerMoved(target), primary(target, false)],
                CONTENT_TOP,
                true,
                true,
            );
            assert!(released.dropped.is_none());
            assert!(!DragAndDrop::has_payload_of_type::<TrackDrag>(&ctx));
        }
    }

    #[test]
    fn escape_and_outside_release_cancel_without_reordering() {
        for escape in [false, true] {
            let ctx = egui::Context::default();
            crate::ui_style::configure_style(&ctx).expect("system fonts");
            let project = project(6);
            let target = egui::pos2(150.0, 205.0);
            start_drag(&ctx, &project, target);
            let release_at = if escape {
                let cancelled = frame(
                    &ctx,
                    &project,
                    vec![egui::Event::Key {
                        key: egui::Key::Escape,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    }],
                    CONTENT_TOP,
                    true,
                    true,
                );
                assert!(cancelled.dropped.is_none());
                assert!(!DragAndDrop::has_payload_of_type::<TrackDrag>(&ctx));
                target
            } else {
                egui::pos2(10.0, 205.0)
            };
            let released = frame(
                &ctx,
                &project,
                vec![
                    egui::Event::PointerMoved(release_at),
                    primary(release_at, false),
                ],
                CONTENT_TOP,
                true,
                true,
            );
            assert!(released.dropped.is_none());
            assert!(!DragAndDrop::has_payload_of_type::<TrackDrag>(&ctx));
        }
    }

    #[test]
    fn nonprimary_release_does_not_commit_a_reorder() {
        let ctx = egui::Context::default();
        crate::ui_style::configure_style(&ctx).expect("system fonts");
        let project = project(6);
        let target = egui::pos2(150.0, 205.0);
        start_drag(&ctx, &project, target);
        let released = frame(
            &ctx,
            &project,
            vec![
                button(target, egui::PointerButton::Secondary, true),
                button(target, egui::PointerButton::Secondary, false),
            ],
            CONTENT_TOP,
            true,
            true,
        );
        assert!(released.dropped.is_none());
        assert!(!DragAndDrop::has_payload_of_type::<TrackDrag>(&ctx));
        assert!(
            frame(
                &ctx,
                &project,
                vec![primary(target, false)],
                CONTENT_TOP,
                true,
                true
            )
            .dropped
            .is_none()
        );
    }

    #[test]
    fn disabled_stale_and_unfocused_targets_reject_and_clear_payloads() {
        for case in ["disabled", "other-project", "removed-track", "unfocused"] {
            let ctx = egui::Context::default();
            crate::ui_style::configure_style(&ctx).expect("system fonts");
            let project = project(6);
            frame(&ctx, &project, vec![], CONTENT_TOP, true, true);
            DragAndDrop::set_payload(
                &ctx,
                TrackDrag {
                    project_id: if case == "other-project" {
                        Uuid::new_v4()
                    } else {
                        project.id
                    },
                    track_id: if case == "removed-track" {
                        Uuid::new_v4()
                    } else {
                        project.tracks[1].id
                    },
                },
            );
            let result = frame(
                &ctx,
                &project,
                vec![egui::Event::PointerMoved(egui::pos2(150.0, 205.0))],
                CONTENT_TOP,
                case != "disabled",
                case != "unfocused",
            );
            assert!(result.dropped.is_none(), "{case}");
            assert!(
                !DragAndDrop::has_payload_of_type::<TrackDrag>(&ctx),
                "{case}"
            );
        }
    }

    #[test]
    fn scroll_speed_points_toward_edges_and_stops_outside_viewport() {
        let viewport = viewport();
        assert_eq!(scroll_speed(egui::pos2(200.0, 40.0), viewport), 480.0);
        assert_eq!(scroll_speed(egui::pos2(200.0, 280.0), viewport), -480.0);
        assert_eq!(scroll_speed(viewport.center(), viewport), 0.0);
        assert_eq!(scroll_speed(egui::pos2(200.0, 72.0), viewport), 0.0);
        assert_eq!(scroll_speed(egui::pos2(200.0, 248.0), viewport), 0.0);
        for pointer in [
            egui::pos2(19.0, 41.0),
            egui::pos2(381.0, 279.0),
            egui::pos2(200.0, 39.0),
            egui::pos2(200.0, 281.0),
        ] {
            assert_eq!(scroll_speed(pointer, viewport), 0.0);
        }
        assert_eq!(
            scroll_speed(Pos2::ZERO, Rect::from_min_max(Pos2::ZERO, Pos2::ZERO)),
            0.0
        );
    }

    #[test]
    fn auto_scroll_requires_a_matching_enabled_drag_with_primary_held() {
        for (pointer, enabled, matching, held, direction) in [
            (egui::pos2(200.0, 41.0), true, true, true, -1),
            (egui::pos2(200.0, 279.0), true, true, true, 1),
            (egui::pos2(200.0, 41.0), false, true, true, 0),
            (egui::pos2(200.0, 41.0), true, false, true, 0),
            (egui::pos2(200.0, 41.0), true, true, false, 0),
            (egui::pos2(19.0, 41.0), true, true, true, 0),
            (egui::pos2(200.0, 160.0), true, true, true, 0),
        ] {
            let ctx = egui::Context::default();
            crate::ui_style::configure_style(&ctx).expect("system fonts");
            let project_id = Uuid::new_v4();
            DragAndDrop::set_payload(
                &ctx,
                TrackDrag {
                    project_id: if matching { project_id } else { Uuid::new_v4() },
                    track_id: Uuid::new_v4(),
                },
            );
            let mut events = vec![egui::Event::PointerMoved(pointer)];
            if held {
                events.push(primary(pointer, true));
            }
            let mut offset = None;
            ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 320.0))),
                    events,
                    focused: true,
                    ..Default::default()
                },
                |ui| {
                    let scroll = egui::ScrollArea::vertical()
                        .vertical_scroll_offset(200.0)
                        .animated(false)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.set_min_height(1000.0);
                            auto_scroll(ui, project_id, viewport(), enabled);
                        });
                    offset = Some(scroll.state.offset.y);
                },
            )
            .drop_without_applying_deltas();
            let actual = offset.unwrap();
            match direction {
                -1 => assert!(actual < 200.0, "{actual}"),
                1 => assert!(actual > 200.0, "{actual}"),
                _ => assert_eq!(actual, 200.0),
            }
        }
    }
}

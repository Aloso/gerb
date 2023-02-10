/*
 * gerb
 *
 * Copyright 2022 - Manos Pitsidianakis
 *
 * This file is part of gerb.
 *
 * gerb is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * gerb is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with gerb. If not, see <http://www.gnu.org/licenses/>.
 */

use glib::{
    clone, ParamFlags, ParamSpec, ParamSpecBoolean, ParamSpecDouble, ParamSpecString, Value,
};
use gtk::cairo::Matrix;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gio, glib};
use indexmap::IndexMap;
use once_cell::unsync::OnceCell;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use crate::glyphs::{Contour, Glyph, GlyphDrawingOptions, GlyphPointIndex, Guideline};
use crate::project::Project;
use crate::utils::Point;
use crate::views::{
    canvas::{Layer, LayerBuilder},
    overlay::Child,
    Canvas, Overlay, Transformation, UnitPoint, ViewPoint,
};
use crate::{Settings, Workspace};

mod layers;
mod menu;
mod tools;

use tools::{PanningTool, SelectionModifier, Tool, ToolImpl};

#[derive(Debug, Clone)]
pub struct GlyphState {
    pub app: gtk::Application,
    pub glyph: Rc<RefCell<Glyph>>,
    pub reference: Rc<RefCell<Glyph>>,
    pub viewport: Canvas,
    pub tools: IndexMap<glib::types::Type, ToolImpl>,
    pub active_tool: glib::types::Type,
    pub panning_tool: glib::types::Type,
    selection: Vec<GlyphPointIndex>,
    selection_set: HashSet<uuid::Uuid>,
    pub kd_tree: Rc<RefCell<crate::utils::range_query::KdTree>>,
}

impl GlyphState {
    fn new(glyph: &Rc<RefCell<Glyph>>, app: gtk::Application, viewport: Canvas) -> Self {
        let mut ret = Self {
            app,
            glyph: Rc::new(RefCell::new(glyph.borrow().clone())),
            reference: Rc::clone(glyph),
            viewport,
            tools: IndexMap::default(),
            active_tool: glib::types::Type::INVALID,
            panning_tool: PanningTool::static_type(),
            selection: vec![],
            selection_set: HashSet::new(),
            kd_tree: Rc::new(RefCell::new(crate::utils::range_query::KdTree::new(&[]))),
        };

        for (contour_index, contour) in glyph.borrow().contours.iter().enumerate() {
            (ret.add_contour(contour, contour_index).redo)();
        }
        ret
    }

    fn add_contour(&mut self, contour: &Contour, contour_index: usize) -> crate::Action {
        crate::Action {
            stamp: crate::EventStamp {
                t: std::any::TypeId::of::<Self>(),
                property: Contour::static_type().name(),
                id: unsafe { std::mem::transmute::<&[usize], &[u8]>(&[contour_index]).into() },
            },
            compress: false,
            redo: Box::new(
                clone!(@weak self.kd_tree as kd_tree, @weak contour as contour  => move || {
                    let mut kd_tree = kd_tree.borrow_mut();
                    for (curve_index, curve) in contour.curves().borrow().iter().enumerate() {
                        for (idx, pos) in curve.points().borrow().iter().map(|p| (p.glyph_index(contour_index, curve_index), p.position)) {
                            kd_tree.add(idx, pos);
                        }
                    }
                }),
            ),
            undo: Box::new(
                clone!(@weak self.kd_tree as kd_tree, @weak contour as contour => move || {
                    let mut kd_tree = kd_tree.borrow_mut();
                    for (curve_index, curve) in contour.curves().borrow().iter().enumerate() {
                        for idx in curve.points().borrow().iter().map(|p| p.glyph_index(contour_index, curve_index)) {
                            kd_tree.remove(idx);
                        }
                    }
                }),
            ),
        }
    }

    fn new_guideline(&self, angle: f64, p: Point) -> crate::Action {
        let (x, y) = (p.x, p.y);
        let viewport = self.viewport.clone();
        let guideline = Guideline::builder()
            .angle(angle)
            .x(x)
            .y(y)
            .with_random_identifier()
            .build();
        crate::Action {
            stamp: crate::EventStamp {
                t: std::any::TypeId::of::<Self>(),
                property: Guideline::static_type().name(),
                id: Box::new([]),
            },
            compress: false,
            redo: Box::new(
                clone!(@weak self.glyph as glyph, @weak viewport, @strong guideline => move || {
                    glyph.borrow_mut().guidelines.push(guideline.clone());
                    viewport.queue_draw();
                }),
            ),
            undo: Box::new(
                clone!(@weak self.glyph as glyph, @weak viewport => move || {
                    glyph.borrow_mut().guidelines.pop();
                    viewport.queue_draw();
                }),
            ),
        }
    }

    fn delete_guideline(&self, idx: usize) -> crate::Action {
        let viewport = self.viewport.clone();
        let json: serde_json::Value =
            { serde_json::to_value(self.glyph.borrow().guidelines[idx].imp()).unwrap() };
        crate::Action {
            stamp: crate::EventStamp {
                t: std::any::TypeId::of::<Self>(),
                property: Guideline::static_type().name(),
                id: unsafe { std::mem::transmute::<&[usize], &[u8]>(&[idx]).into() },
            },
            compress: false,
            redo: Box::new(
                clone!(@weak self.glyph as glyph, @weak viewport => move || {
                    glyph.borrow_mut().guidelines.remove(idx);
                    viewport.queue_draw();
                }),
            ),
            undo: Box::new(
                clone!(@weak self.glyph as glyph, @weak viewport => move || {
                    glyph.borrow_mut().guidelines.push(Guideline::try_from(json.clone()).unwrap());
                    viewport.queue_draw();
                }),
            ),
        }
    }

    fn add_undo_action(&self, action: crate::Action) {
        let app: &crate::Application = self.app.downcast_ref::<crate::Application>().unwrap();
        app.imp().undo_db.borrow_mut().event(action);
    }

    fn transform_guideline(&self, idx: usize, m: Matrix, dangle: f64) {
        let viewport = self.viewport.clone();
        let mut action = crate::Action {
            stamp: crate::EventStamp {
                t: std::any::TypeId::of::<Self>(),
                property: Guideline::static_type().name(),
                id: unsafe { std::mem::transmute::<&[usize], &[u8]>(&[idx]).into() },
            },
            compress: true,
            redo: Box::new(
                clone!(@weak self.glyph as glyph, @weak viewport => move || {
                    let glyph = glyph.borrow();
                    let g = &glyph.guidelines[idx];
                    let x = g.property(Guideline::X);
                    let y = g.property(Guideline::Y);
                    let angle: f64 = g.property(Guideline::ANGLE);
                    let (x, y) = m.transform_point(x, y);
                    g.set_property(Guideline::X, x);
                    g.set_property(Guideline::Y, y);
                    g.set_property(Guideline::ANGLE, angle + dangle);
                    viewport.queue_draw();
                }),
            ),
            undo: Box::new(
                clone!(@weak self.glyph as glyph, @weak viewport => move || {
                    let m = if let Ok(m) = m.try_invert() {m} else {return;};
                    let glyph = glyph.borrow();
                    let g = &glyph.guidelines[idx];
                    let x = g.property(Guideline::X);
                    let y = g.property(Guideline::Y);
                    let angle: f64 = g.property(Guideline::ANGLE);
                    let (x, y) = m.transform_point(x, y);
                    g.set_property(Guideline::X, x);
                    g.set_property(Guideline::Y, y);
                    g.set_property(Guideline::ANGLE, angle - dangle);
                    viewport.queue_draw();
                }),
            ),
        };
        (action.redo)();
        self.add_undo_action(action);
    }

    fn transform_selection(&self, m: Matrix, compress: bool) {
        let mut action = self.transform_points(&self.selection, m);
        action.compress = compress;
        (action.redo)();
        self.add_undo_action(action);
    }

    fn transform_points(&self, idxs_: &[GlyphPointIndex], m: Matrix) -> crate::Action {
        let viewport = self.viewport.clone();
        let idxs = Rc::new(idxs_.to_vec());
        crate::Action {
            stamp: crate::EventStamp {
                t: std::any::TypeId::of::<Self>(),
                property: Point::static_type().name(),
                id: idxs_
                    .iter()
                    .map(GlyphPointIndex::as_bytes)
                    .flat_map(<_>::into_iter)
                    .collect::<Vec<u8>>()
                    .into(),
            },
            compress: false,
            redo: Box::new(
                clone!(@strong idxs, @weak self.kd_tree as kd_tree, @weak self.glyph as glyph, @weak viewport => move || {
                    let mut kd_tree = kd_tree.borrow_mut();
                    let glyph = glyph.borrow();
                    for contour_index in idxs.iter().map(|i| i.contour_index).collect::<HashSet<usize>>() {
                        let contour = &glyph.contours[contour_index];
                        for (idx, new_pos) in contour.transform_points(contour_index, &idxs, m) {
                            kd_tree.add(idx, new_pos);
                        }
                    }
                    viewport.queue_draw();
                }),
            ),
            undo: Box::new(
                clone!(@strong idxs, @weak self.kd_tree as kd_tree, @weak self.glyph as glyph, @weak viewport => move || {
                    let m = if let Ok(m) = m.try_invert() {m} else {return;};
                    let mut kd_tree = kd_tree.borrow_mut();
                    let glyph = glyph.borrow();
                    for contour_index in idxs.iter().map(|i| i.contour_index).collect::<HashSet<usize>>() {
                        let contour = &glyph.contours[contour_index];
                        for (idx, new_pos) in contour.transform_points(contour_index, &idxs, m) {
                            /* update kd_tree */
                            kd_tree.add(idx, new_pos);
                        }
                    }
                    viewport.queue_draw();
                }),
            ),
        }
    }

    fn set_selection(&mut self, selection: &[GlyphPointIndex], modifier: SelectionModifier) {
        use SelectionModifier::*;
        match modifier {
            Replace => {
                self.selection.clear();
                self.selection_set.clear();
                self.selection.extend(selection.iter());
                for v in &self.selection {
                    self.selection_set.insert(v.uuid);
                }
            }
            Add => {
                self.selection.extend(selection.iter());
                for v in &self.selection {
                    self.selection_set.insert(v.uuid);
                }
            }
            Remove => {
                self.selection.retain(|e| !selection.contains(e));
                for v in selection {
                    self.selection_set.remove(&v.uuid);
                }
            }
        }
    }

    fn get_selection(&self) -> &HashSet<uuid::Uuid> {
        &self.selection_set
    }
}

type StatusBarMessage = u32;

#[derive(Debug, Default)]
pub struct GlyphEditViewInner {
    app: OnceCell<gtk::Application>,
    project: OnceCell<Project>,
    glyph: OnceCell<Rc<RefCell<Glyph>>>,
    glyph_state: OnceCell<Rc<RefCell<GlyphState>>>,
    viewport: Canvas,
    statusbar_context_id: Cell<Option<u32>>,
    overlay: Overlay,
    hovering: Cell<Option<(usize, usize)>>,
    pub toolbar_box: gtk::Box,
    units_per_em: Cell<f64>,
    descender: Cell<f64>,
    x_height: Cell<f64>,
    cap_height: Cell<f64>,
    ascender: Cell<f64>,
    lock_guidelines: Cell<bool>,
    show_glyph_guidelines: Cell<bool>,
    show_project_guidelines: Cell<bool>,
    show_metrics_guidelines: Cell<bool>,
    settings: OnceCell<Settings>,
    menubar: gtk::MenuBar,
    preview: Cell<Option<StatusBarMessage>>,
    ctrl: OnceCell<gtk::EventControllerKey>,
    action_group: gio::SimpleActionGroup,
    lock: Cell<(Option<StatusBarMessage>, tools::constraints::Lock)>,
    snap: Cell<(Option<StatusBarMessage>, tools::constraints::Snap)>,
}

#[glib::object_subclass]
impl ObjectSubclass for GlyphEditViewInner {
    const NAME: &'static str = "GlyphEditView";
    type Type = GlyphEditView;
    type ParentType = gtk::Bin;
}

impl ObjectImpl for GlyphEditViewInner {
    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);
        self.lock_guidelines.set(true);
        let ctrl = gtk::EventControllerKey::new(obj);
        ctrl.connect_key_pressed(
            clone!(@weak self.action_group as group => @default-return false, move |_self, keyval, _, _| {
                use gtk::gdk::keys::{Key, constants as c};
                use GlyphEditView as A;

                let key = Key::from(keyval);
                match key {
                    c::grave if group.is_action_enabled(A::PREVIEW_ACTION) => {
                        group.change_action_state(A::PREVIEW_ACTION, &true.to_variant());
                    },
                    c::x if group.is_action_enabled(A::LOCK_ACTION) => {
                        group.activate_action(A::LOCK_X_ACTION, None);
                    }
                    c::y if group.is_action_enabled(A::LOCK_ACTION) => {
                        group.activate_action(A::LOCK_Y_ACTION, None);
                    }
                    c::A if group.is_action_enabled(A::SNAP_ACTION) => {
                        group.activate_action(A::SNAP_ANGLE_ACTION, None);
                    }
                    c::G if group.is_action_enabled(A::SNAP_ACTION) => {
                        group.activate_action(A::SNAP_GRID_ACTION, None);
                    }
                    c::L if group.is_action_enabled(A::SNAP_ACTION) => {
                        group.activate_action(A::SNAP_GUIDELINES_ACTION, None);
                    }
                    c::M if group.is_action_enabled(A::SNAP_ACTION) => {
                        group.activate_action(A::SNAP_METRICS_ACTION, None);
                    }
                    _ => return false,
                }
                true
            }),
        );
        ctrl.connect_key_released(
            clone!(@weak self.action_group as group => move |_self, keyval, _,  _| {
                use gtk::gdk::keys::{Key, constants as c};
                use GlyphEditView as A;

                let key = Key::from(keyval);
                match key {
                    c::grave if group.is_action_enabled(A::PREVIEW_ACTION) => {
                        group.change_action_state(A::PREVIEW_ACTION, &false.to_variant());
                    },
                    _ => {},
                }
            }),
        );
        self.ctrl.set(ctrl).unwrap();
        self.show_glyph_guidelines.set(true);
        self.show_project_guidelines.set(true);
        self.show_metrics_guidelines.set(true);
        self.statusbar_context_id.set(None);
        self.viewport.set_mouse(ViewPoint((0.0, 0.0).into()));

        self.viewport.connect_scroll_event(
            clone!(@weak obj => @default-return Inhibit(false), move |viewport, event| {
                let retval = Tool::on_scroll_event(obj, viewport, event);
                if retval == Inhibit(true) {
                    viewport.queue_draw();
                }
                retval
            }),
        );

        self.viewport.connect_button_press_event(
            clone!(@weak obj => @default-return Inhibit(false), move |viewport, event| {
                let retval = Tool::on_button_press_event(obj, viewport, event);
                if retval == Inhibit(true) {
                    viewport.queue_draw();
                }
                viewport.set_mouse(ViewPoint(event.position().into()));
                retval
            }),
        );

        self.viewport.connect_button_release_event(
            clone!(@weak obj => @default-return Inhibit(false), move |viewport, event| {
                let retval = Tool::on_button_release_event(obj, viewport, event);
                if retval == Inhibit(true) {
                    viewport.queue_draw();
                }
                viewport.set_mouse(ViewPoint(event.position().into()));
                retval
            }),
        );

        self.viewport.connect_motion_notify_event(
            clone!(@weak obj => @default-return Inhibit(false), move |viewport, event| {
                let retval = Tool::on_motion_notify_event(obj, viewport, event);
                viewport.set_mouse(ViewPoint(event.position().into()));
                if let Inhibit(true) = retval {
                    viewport.queue_draw();
                }
                retval
            }),
        );

        self.viewport.add_layer(
            LayerBuilder::new()
                .set_name(Some("glyph"))
                .set_active(true)
                .set_hidden(false)
                .set_callback(Some(Box::new(clone!(@weak obj => @default-return Inhibit(false), move |viewport: &Canvas, cr: &gtk::cairo::Context| {
                    layers::draw_glyph_layer(viewport, cr, obj)
                }))))
                .build(),
        );
        self.viewport.add_pre_layer(
            LayerBuilder::new()
                .set_name(Some("guidelines"))
                .set_active(true)
                .set_hidden(true)
                .set_callback(Some(Box::new(clone!(@weak obj => @default-return Inhibit(false), move |viewport: &Canvas, cr: &gtk::cairo::Context| {
                    layers::draw_guidelines(viewport, cr, obj)
                }))))
                .build(),
        );
        self.viewport.add_post_layer(
            LayerBuilder::new()
                .set_name(Some("rules"))
                .set_active(true)
                .set_hidden(true)
                .set_callback(Some(Box::new(Canvas::draw_rulers)))
                .build(),
        );
        self.overlay.set_child(&self.viewport);
        self.overlay
            .add_overlay(Child::new(self.toolbar_box.clone()));
        self.overlay
            .add_overlay(Child::new(self.create_layer_widget()).expanded(false));
        obj.add(&self.overlay);
        obj.set_visible(true);
        obj.set_expand(true);
        obj.set_can_focus(true);
    }

    fn properties() -> &'static [ParamSpec] {
        static PROPERTIES: once_cell::sync::Lazy<Vec<ParamSpec>> =
            once_cell::sync::Lazy::new(|| {
                vec![
                    ParamSpecString::new(
                        GlyphEditView::TITLE,
                        GlyphEditView::TITLE,
                        GlyphEditView::TITLE,
                        Some("edit glyph"),
                        ParamFlags::READABLE,
                    ),
                    ParamSpecBoolean::new(
                        GlyphEditView::CLOSEABLE,
                        GlyphEditView::CLOSEABLE,
                        GlyphEditView::CLOSEABLE,
                        true,
                        ParamFlags::READABLE,
                    ),
                    ParamSpecBoolean::new(
                        GlyphEditView::PREVIEW,
                        GlyphEditView::PREVIEW,
                        GlyphEditView::PREVIEW,
                        false,
                        ParamFlags::READWRITE,
                    ),
                    ParamSpecBoolean::new(
                        GlyphEditView::IS_MENU_VISIBLE,
                        GlyphEditView::IS_MENU_VISIBLE,
                        GlyphEditView::IS_MENU_VISIBLE,
                        true,
                        ParamFlags::READABLE,
                    ),
                    ParamSpecDouble::new(
                        GlyphEditView::UNITS_PER_EM,
                        GlyphEditView::UNITS_PER_EM,
                        GlyphEditView::UNITS_PER_EM,
                        1.0,
                        std::f64::MAX,
                        1000.0,
                        ParamFlags::READWRITE,
                    ),
                    ParamSpecDouble::new(
                        GlyphEditView::X_HEIGHT,
                        GlyphEditView::X_HEIGHT,
                        GlyphEditView::X_HEIGHT,
                        1.0,
                        std::f64::MAX,
                        1000.0,
                        ParamFlags::READWRITE,
                    ),
                    ParamSpecDouble::new(
                        GlyphEditView::ASCENDER,
                        GlyphEditView::ASCENDER,
                        GlyphEditView::ASCENDER,
                        std::f64::MIN,
                        std::f64::MAX,
                        700.0,
                        ParamFlags::READWRITE,
                    ),
                    ParamSpecDouble::new(
                        GlyphEditView::DESCENDER,
                        GlyphEditView::DESCENDER,
                        GlyphEditView::DESCENDER,
                        std::f64::MIN,
                        std::f64::MAX,
                        -200.0,
                        ParamFlags::READWRITE,
                    ),
                    ParamSpecDouble::new(
                        GlyphEditView::CAP_HEIGHT,
                        GlyphEditView::CAP_HEIGHT,
                        GlyphEditView::CAP_HEIGHT,
                        std::f64::MIN,
                        std::f64::MAX,
                        650.0,
                        ParamFlags::READWRITE,
                    ),
                    ParamSpecBoolean::new(
                        GlyphEditView::LOCK_GUIDELINES,
                        GlyphEditView::LOCK_GUIDELINES,
                        GlyphEditView::LOCK_GUIDELINES,
                        false,
                        ParamFlags::READWRITE,
                    ),
                    ParamSpecBoolean::new(
                        GlyphEditView::SHOW_GLYPH_GUIDELINES,
                        GlyphEditView::SHOW_GLYPH_GUIDELINES,
                        GlyphEditView::SHOW_GLYPH_GUIDELINES,
                        true,
                        ParamFlags::READWRITE,
                    ),
                    ParamSpecBoolean::new(
                        GlyphEditView::SHOW_PROJECT_GUIDELINES,
                        GlyphEditView::SHOW_PROJECT_GUIDELINES,
                        GlyphEditView::SHOW_PROJECT_GUIDELINES,
                        true,
                        ParamFlags::READWRITE,
                    ),
                    ParamSpecBoolean::new(
                        GlyphEditView::SHOW_METRICS_GUIDELINES,
                        GlyphEditView::SHOW_METRICS_GUIDELINES,
                        GlyphEditView::SHOW_METRICS_GUIDELINES,
                        true,
                        ParamFlags::READWRITE,
                    ),
                    glib::ParamSpecObject::new(
                        GlyphEditView::ACTIVE_TOOL,
                        GlyphEditView::ACTIVE_TOOL,
                        GlyphEditView::ACTIVE_TOOL,
                        ToolImpl::static_type(),
                        glib::ParamFlags::READWRITE,
                    ),
                    glib::ParamSpecObject::new(
                        GlyphEditView::PANNING_TOOL,
                        GlyphEditView::PANNING_TOOL,
                        GlyphEditView::PANNING_TOOL,
                        ToolImpl::static_type(),
                        glib::ParamFlags::READWRITE,
                    ),
                    glib::ParamSpecObject::new(
                        Workspace::MENUBAR,
                        Workspace::MENUBAR,
                        Workspace::MENUBAR,
                        gtk::MenuBar::static_type(),
                        ParamFlags::READWRITE,
                    ),
                    glib::ParamSpecUInt::new(
                        GlyphEditView::LOCK,
                        GlyphEditView::LOCK,
                        "Lock transformation movement to specific axes.",
                        0,
                        u32::MAX,
                        0,
                        ParamFlags::READWRITE,
                    ),
                    glib::ParamSpecUInt::new(
                        GlyphEditView::SNAP,
                        GlyphEditView::SNAP,
                        "Snap transformation movement to specific references.",
                        0,
                        u32::MAX,
                        0,
                        ParamFlags::READWRITE,
                    ),
                ]
            });
        PROPERTIES.as_ref()
    }

    fn property(&self, obj: &Self::Type, _id: usize, pspec: &ParamSpec) -> Value {
        match pspec.name() {
            GlyphEditView::TITLE => {
                if let Some(name) = obj
                    .imp()
                    .glyph_state
                    .get()
                    .map(|s| s.borrow().glyph.borrow().name_markup())
                {
                    format!("edit <i>{}</i>", name).to_value()
                } else {
                    "edit glyph".to_value()
                }
            }
            GlyphEditView::CLOSEABLE => true.to_value(),
            GlyphEditView::PREVIEW => self.preview.get().is_some().to_value(),
            GlyphEditView::IS_MENU_VISIBLE => true.to_value(),
            GlyphEditView::UNITS_PER_EM => self.units_per_em.get().to_value(),
            GlyphEditView::X_HEIGHT => self.x_height.get().to_value(),
            GlyphEditView::ASCENDER => self.ascender.get().to_value(),
            GlyphEditView::DESCENDER => self.descender.get().to_value(),
            GlyphEditView::CAP_HEIGHT => self.cap_height.get().to_value(),
            GlyphEditView::LOCK_GUIDELINES => self.lock_guidelines.get().to_value(),
            GlyphEditView::SHOW_GLYPH_GUIDELINES => self.show_glyph_guidelines.get().to_value(),
            GlyphEditView::SHOW_PROJECT_GUIDELINES => self.show_project_guidelines.get().to_value(),
            GlyphEditView::SHOW_METRICS_GUIDELINES => self.show_metrics_guidelines.get().to_value(),
            GlyphEditView::ACTIVE_TOOL => {
                let state = self.glyph_state.get().unwrap().borrow();
                let active_tool = state.active_tool;
                state.tools.get(&active_tool).map(Clone::clone).to_value()
            }
            GlyphEditView::PANNING_TOOL => {
                let state = self.glyph_state.get().unwrap().borrow();
                let panning_tool = state.panning_tool;
                state.tools.get(&panning_tool).map(Clone::clone).to_value()
            }
            GlyphEditView::MENUBAR => Some(self.menubar.clone()).to_value(),
            GlyphEditView::LOCK => self.lock.get().1.bits().to_value(),
            GlyphEditView::SNAP => self.snap.get().1.bits().to_value(),
            _ => unimplemented!("{}", pspec.name()),
        }
    }

    fn set_property(&self, _obj: &Self::Type, _id: usize, value: &Value, pspec: &ParamSpec) {
        match pspec.name() {
            GlyphEditView::UNITS_PER_EM => {
                self.units_per_em.set(value.get().unwrap());
            }
            GlyphEditView::X_HEIGHT => {
                self.x_height.set(value.get().unwrap());
            }
            GlyphEditView::ASCENDER => {
                self.ascender.set(value.get().unwrap());
            }
            GlyphEditView::DESCENDER => {
                self.descender.set(value.get().unwrap());
            }
            GlyphEditView::CAP_HEIGHT => {
                self.cap_height.set(value.get().unwrap());
            }
            GlyphEditView::LOCK_GUIDELINES => {
                self.lock_guidelines.set(value.get().unwrap());
            }
            GlyphEditView::SHOW_GLYPH_GUIDELINES => {
                self.show_glyph_guidelines.set(value.get().unwrap());
            }
            GlyphEditView::SHOW_PROJECT_GUIDELINES => {
                self.show_project_guidelines.set(value.get().unwrap());
            }
            GlyphEditView::SHOW_METRICS_GUIDELINES => {
                self.show_metrics_guidelines.set(value.get().unwrap());
            }
            GlyphEditView::PREVIEW => {
                let v: bool = value.get().unwrap();
                if let Some(mid) = self.preview.get() {
                    if v {
                        return;
                    }
                    self.pop_statusbar_message(Some(mid));
                    self.preview.set(None);
                } else {
                    if !v {
                        return;
                    }
                    self.preview.set(self.new_statusbar_message("Preview."));
                }
                self.viewport.queue_draw();
            }
            GlyphEditView::LOCK => {
                if let Some(v) = value
                    .get::<u32>()
                    .ok()
                    .and_then(tools::constraints::Lock::from_bits)
                {
                    let (msg, _) = self.lock.get();
                    if msg.is_some() {
                        self.pop_statusbar_message(msg);
                    }
                    let new_msg = if v.is_empty() {
                        None
                    } else {
                        self.new_statusbar_message(v.as_str())
                    };
                    self.lock.set((new_msg, v));
                    self.viewport.queue_draw();
                }
            }
            GlyphEditView::SNAP => {
                if let Some(v) = value
                    .get::<u32>()
                    .ok()
                    .and_then(tools::constraints::Snap::from_bits)
                {
                    let (msg, _) = self.snap.get();
                    if msg.is_some() {
                        self.pop_statusbar_message(msg);
                    }
                    let new_msg = if v.is_empty() {
                        None
                    } else {
                        self.new_statusbar_message(v.as_str())
                    };
                    self.snap.set((new_msg, v));
                    self.viewport.queue_draw();
                }
            }
            _ => unimplemented!("{}", pspec.name()),
        }
    }
}

impl WidgetImpl for GlyphEditViewInner {}
impl ContainerImpl for GlyphEditViewInner {}
impl BinImpl for GlyphEditViewInner {}

impl GlyphEditViewInner {
    fn new_statusbar_message(&self, msg: &str) -> Option<StatusBarMessage> {
        if let Some(app) = self
            .app
            .get()
            .and_then(|app| app.downcast_ref::<crate::Application>())
        {
            let statusbar = app.imp().statusbar();
            if self.statusbar_context_id.get().is_none() {
                self.statusbar_context_id.set(Some(
                    statusbar
                        .context_id(&format!("GlyphEditView-{:?}", &self.glyph.get().unwrap())),
                ));
            }
            if let Some(cid) = self.statusbar_context_id.get().as_ref() {
                return Some(statusbar.push(*cid, msg));
            }
        }
        None
    }

    fn pop_statusbar_message(&self, msg: Option<StatusBarMessage>) {
        if let Some(app) = self
            .app
            .get()
            .and_then(|app| app.downcast_ref::<crate::Application>())
        {
            let statusbar = app.imp().statusbar();
            if let Some(cid) = self.statusbar_context_id.get().as_ref() {
                if let Some(mid) = msg {
                    gtk::prelude::StatusbarExt::remove(&statusbar, *cid, mid);
                } else {
                    statusbar.pop(*cid);
                }
            }
        }
    }

    fn select_object(&self, _new_obj: Option<glib::Object>) {
        if let Some(_app) = self
            .app
            .get()
            .and_then(|app| app.downcast_ref::<crate::Application>())
        {
            //let tabinfo = app.tabinfo();
            //tabinfo.set_object(new_obj);
        }
    }
}

glib::wrapper! {
    pub struct GlyphEditView(ObjectSubclass<GlyphEditViewInner>)
        @extends gtk::Widget, gtk::Container, gtk::Bin;
}

impl GlyphEditView {
    pub const ASCENDER: &str = Project::ASCENDER;
    pub const CAP_HEIGHT: &str = Project::CAP_HEIGHT;
    pub const CLOSEABLE: &str = "closeable";
    pub const PREVIEW: &str = "preview";
    pub const DESCENDER: &str = Project::DESCENDER;
    pub const TITLE: &str = "title";
    pub const IS_MENU_VISIBLE: &str = Workspace::IS_MENU_VISIBLE;
    pub const MENUBAR: &str = Workspace::MENUBAR;
    pub const UNITS_PER_EM: &str = Project::UNITS_PER_EM;
    pub const X_HEIGHT: &str = Project::X_HEIGHT;
    pub const LOCK_GUIDELINES: &str = "lock-guidelines";
    pub const SHOW_GLYPH_GUIDELINES: &str = "show-glyph-guidelines";
    pub const SHOW_PROJECT_GUIDELINES: &str = "show-project-guidelines";
    pub const SHOW_METRICS_GUIDELINES: &str = "show-metrics-guidelines";
    pub const ACTIVE_TOOL: &str = "active-tool";
    pub const PANNING_TOOL: &str = "panning-tool";
    pub const LOCK: &str = "lock";
    pub const SNAP: &str = "snap";
    pub const PREVIEW_ACTION: &str = Self::PREVIEW;
    pub const ZOOM_IN_ACTION: &str = "zoom.in";
    pub const ZOOM_OUT_ACTION: &str = "zoom.out";
    pub const LOCK_ACTION: &str = Self::LOCK;
    pub const LOCK_X_ACTION: &str = "lock.x";
    pub const LOCK_Y_ACTION: &str = "lock.y";
    pub const LOCK_LOCAL_ACTION: &str = "lock.local";
    pub const LOCK_CONTROLS_ACTION: &str = "lock.controls";
    pub const PRECISION_ACTION: &str = "precision";
    pub const SNAP_ACTION: &str = Self::SNAP;
    pub const SNAP_CLEAR_ACTION: &str = "snap.clear";
    pub const SNAP_ANGLE_ACTION: &str = "snap.angle";
    pub const SNAP_GRID_ACTION: &str = "snap.grid";
    pub const SNAP_GUIDELINES_ACTION: &str = "snap.guidelines";
    pub const SNAP_METRICS_ACTION: &str = "snap.metrics";

    pub fn new(app: gtk::Application, project: Project, glyph: Rc<RefCell<Glyph>>) -> Self {
        let ret: Self = glib::Object::new(&[]).unwrap();
        ret.imp().glyph.set(glyph.clone()).unwrap();
        ret.imp().app.set(app.clone()).unwrap();
        {
            let property = GlyphEditView::UNITS_PER_EM;
            ret.bind_property(property, &ret.imp().viewport.imp().transformation, property)
                .flags(glib::BindingFlags::SYNC_CREATE)
                .build();
        }
        ret.imp().viewport.imp().transformation.set_property(
            Transformation::CONTENT_WIDTH,
            glyph
                .borrow()
                .width
                .unwrap_or_else(|| ret.property::<f64>(GlyphEditView::UNITS_PER_EM)),
        );
        for property in [
            GlyphEditView::ASCENDER,
            GlyphEditView::CAP_HEIGHT,
            GlyphEditView::DESCENDER,
            GlyphEditView::UNITS_PER_EM,
            GlyphEditView::X_HEIGHT,
        ] {
            project
                .bind_property(property, &ret, property)
                .flags(glib::BindingFlags::SYNC_CREATE)
                .build();
        }
        let settings = app
            .downcast_ref::<crate::Application>()
            .unwrap()
            .imp()
            .settings
            .borrow()
            .clone();
        settings
            .bind_property(
                Canvas::WARP_CURSOR,
                &ret.imp().viewport,
                Canvas::WARP_CURSOR,
            )
            .flags(glib::BindingFlags::SYNC_CREATE)
            .build();
        for prop in [Settings::HANDLE_SIZE, Settings::LINE_WIDTH] {
            settings.connect_notify_local(
                Some(prop),
                clone!(@strong ret => move |_self, _| {
                    ret.imp().viewport.queue_draw();
                }),
            );
        }
        ret.imp().settings.set(settings).unwrap();
        for prop in [
            Canvas::SHOW_GRID,
            Canvas::SHOW_GUIDELINES,
            Canvas::SHOW_HANDLES,
            Canvas::INNER_FILL,
            Canvas::SHOW_TOTAL_AREA,
        ] {
            let prop_action = gio::PropertyAction::new(prop, &ret.imp().viewport, prop);
            ret.imp().action_group.add_action(&prop_action);
        }
        {
            let prop_action = gio::PropertyAction::new(Self::PREVIEW_ACTION, &ret, Self::PREVIEW);
            ret.imp().action_group.add_action(&prop_action);
        }
        for (zoom_action, tool_func) in [
            (
                Self::ZOOM_IN_ACTION,
                &Transformation::zoom_in as &dyn Fn(&Transformation) -> bool,
            ),
            (Self::ZOOM_OUT_ACTION, &Transformation::zoom_out),
        ] {
            let action = gio::SimpleAction::new(zoom_action, None);
            action.connect_activate(glib::clone!(@weak ret as obj => move |_, _| {
                let t = &obj.imp().viewport.imp().transformation;
                tool_func(t);
            }));
            ret.imp().action_group.add_action(&action);
        }
        tools::constraints::create_constraint_actions(&ret);
        ret.insert_action_group("view", Some(&ret.imp().action_group));
        ret.imp()
            .menubar
            .insert_action_group("view", Some(&ret.imp().action_group));
        ret.imp()
            .glyph_state
            .set(Rc::new(RefCell::new(GlyphState::new(
                &glyph,
                app,
                ret.imp().viewport.clone(),
            ))))
            .expect("Failed to create glyph state");
        Tool::setup_toolbox(&ret);
        ret.imp().project.set(project).unwrap();
        ret.imp().setup_menu(&ret);
        ret
    }
}

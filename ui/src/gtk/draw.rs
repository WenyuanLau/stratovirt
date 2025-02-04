// Copyright (c) 2023 Huawei Technologies Co.,Ltd. All rights reserved.
//
// StratoVirt is licensed under Mulan PSL v2.
// You can use this software according to the terms and conditions of the Mulan
// PSL v2.
// You may obtain a copy of Mulan PSL v2 at:
//         http://license.coscl.org.cn/MulanPSL2
// THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY
// KIND, EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
// NON-INFRINGEMENT, MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
// See the Mulan PSL v2 for more details.

use std::{cell::RefCell, rc::Rc};

use anyhow::Result;
use gdk::{prelude::SeatExt, SeatCapabilities};
use gtk::{
    cairo,
    gdk::{self, EventMask, ScrollDirection},
    glib::{self, translate::IntoGlib},
    prelude::WidgetExtManual,
    traits::WidgetExt,
    DrawingArea, Inhibit,
};
use log::error;

use crate::{
    console::graphic_hardware_ui_info,
    gtk::GtkDisplayScreen,
    input::{
        self, point_event, press_mouse, release_all_key, update_key_state, ABS_MAX,
        INPUT_BUTTON_WHEEL_DOWN, INPUT_BUTTON_WHEEL_LEFT, INPUT_BUTTON_WHEEL_RIGHT,
        INPUT_BUTTON_WHEEL_UP, INPUT_POINT_LEFT, INPUT_POINT_MIDDLE, INPUT_POINT_RIGHT,
    },
};

pub(crate) fn set_callback_for_draw_area(
    draw_area: &DrawingArea,
    gs: Rc<RefCell<GtkDisplayScreen>>,
) -> Result<()> {
    draw_area.connect_draw(
        glib::clone!(@weak gs => @default-return Inhibit(false), move |_, cr| {
            da_draw_callback(&gs, cr).unwrap_or_else(|e| error!("Draw: {}", e));
            Inhibit(false)
        }),
    );
    draw_area.connect_event(
        glib::clone!(@weak gs => @default-return Inhibit(false), move |_, event| {
            da_event_callback(&gs, event).unwrap_or_else(|e| error!("Draw event: {}", e));
            Inhibit(false)}),
    );
    draw_area.connect_button_press_event(
        glib::clone!(@weak gs => @default-return Inhibit(false), move |_, button_event| {
            da_pointer_callback(&gs, button_event).unwrap_or_else(|e| error!("Press event: {}", e));
            Inhibit(false)
        }),
    );
    draw_area.connect_button_release_event(
        glib::clone!(@weak gs => @default-return Inhibit(false), move |_, button_event| {
            da_pointer_callback(&gs, button_event).unwrap_or_else(|e| error!("Release event: {}", e));
            Inhibit(false)
        }),
    );
    draw_area.connect_scroll_event(
        glib::clone!(@weak gs => @default-return Inhibit(false), move |_, scroll_event| {
            da_scroll_callback(&gs, scroll_event).unwrap_or_else(|e| error!("Scroll event: {}", e));
            Inhibit(false)
        }),
    );
    draw_area.connect_key_press_event(
        glib::clone!(@weak gs => @default-return Inhibit(true), move |_, key_event| {
            da_key_callback(&gs,key_event, true).unwrap_or_else(|e|error!("Press event: {}", e));
            Inhibit(true)}
        ),
    );
    draw_area.connect_key_release_event(
        glib::clone!(@weak gs => @default-return Inhibit(true), move |_, key_event| {
            da_key_callback(&gs,key_event, false).unwrap_or_else(|e|error!("Key event: {}", e));
            Inhibit(true)}
        ),
    );
    draw_area.connect_configure_event(
        glib::clone!(@weak gs => @default-return false, move |_, event_configure| {
            da_configure_callback(&gs, event_configure).unwrap_or_else(|e|error!("Configure event: {}", e));
            false}
        ),
    );

    draw_area.connect_focus_out_event(
        glib::clone!(@weak gs => @default-return Inhibit(false), move |_, _| {
            da_focus_out_callback().unwrap_or_else(|e|error!("Focus out event: {:?}", e));
            Inhibit(false)}
        ),
    );
    draw_area.connect_enter_notify_event(
        glib::clone!(@weak gs => @default-return Inhibit(false), move |_,enter_event| {
            da_enter_callback(&gs, enter_event).unwrap_or_else(|e|error!("Enter event: {:?}", e));
            Inhibit(false)}
        ),
    );
    draw_area.connect_leave_notify_event(
        glib::clone!(@weak gs => @default-return Inhibit(false), move |_, leave_event| {
            da_leave_callback(&gs, leave_event).unwrap_or_else(|e|error!("Leave event: {:?}", e));
            Inhibit(false)}
        ),
    );

    let event_mask = EventMask::BUTTON_PRESS_MASK
        | EventMask::BUTTON_RELEASE_MASK
        | EventMask::BUTTON_MOTION_MASK
        | EventMask::SCROLL_MASK
        | EventMask::SMOOTH_SCROLL_MASK
        | EventMask::KEY_PRESS_MASK
        | EventMask::KEY_RELEASE_MASK
        | EventMask::BUTTON1_MOTION_MASK
        | EventMask::FOCUS_CHANGE_MASK
        | EventMask::ENTER_NOTIFY_MASK
        | EventMask::LEAVE_NOTIFY_MASK
        | EventMask::POINTER_MOTION_MASK;
    draw_area.add_events(event_mask);

    Ok(())
}

fn da_enter_callback(
    gs: &Rc<RefCell<GtkDisplayScreen>>,
    _event: &gdk::EventCrossing,
) -> Result<()> {
    update_keyboard_grab(gs, true);
    Ok(())
}

fn da_leave_callback(
    gs: &Rc<RefCell<GtkDisplayScreen>>,
    _event: &gdk::EventCrossing,
) -> Result<()> {
    update_keyboard_grab(gs, false);
    Ok(())
}

fn update_keyboard_grab(gs: &Rc<RefCell<GtkDisplayScreen>>, grab: bool) {
    let borrowed_gs = gs.borrow();
    let display = borrowed_gs.draw_area.display();
    if let Some(seat) = display.default_seat() {
        if grab {
            if let Some(w) = borrowed_gs.draw_area.window() {
                seat.grab(&w, SeatCapabilities::KEYBOARD, false, None, None, None);
            }
        } else {
            seat.ungrab();
        }
    }
}

/// When the window size changes,
/// the image resolution adapts to the window.
fn da_configure_callback(
    gs: &Rc<RefCell<GtkDisplayScreen>>,
    event_configure: &gdk::EventConfigure,
) -> Result<()> {
    let borrowed_gs = gs.borrow();
    if !borrowed_gs.scale_mode.borrow().is_free_scale() {
        return Ok(());
    }

    let con = match borrowed_gs.con.upgrade() {
        Some(c) => c,
        None => return Ok(()),
    };
    drop(borrowed_gs);
    let (width, height) = event_configure.size();

    graphic_hardware_ui_info(con, width, height)
}

fn da_focus_out_callback() -> Result<()> {
    release_all_key()
}

fn da_key_callback(
    gs: &Rc<RefCell<GtkDisplayScreen>>,
    key_event: &gdk::EventKey,
    press: bool,
) -> Result<()> {
    let keysym2keycode = gs.borrow().keysym2keycode.clone();
    let org_key_value = key_event.keyval().into_glib() as i32;
    let key_value: u16 = key_event.keyval().to_lower().into_glib() as u16;
    let keycode: u16 = match keysym2keycode.borrow().get(&key_value) {
        Some(k) => *k,
        None => 0,
    };
    update_key_state(press, org_key_value, keycode)?;
    input::key_event(keycode, press)?;
    Ok(())
}

fn da_event_callback(gs: &Rc<RefCell<GtkDisplayScreen>>, event: &gdk::Event) -> Result<()> {
    // Cursor movement.
    if event.event_type() == gdk::EventType::MotionNotify {
        gd_cursor_move_event(gs, event).unwrap_or_else(|e| error!("Cursor movement: {:?}", e));
    }
    Ok(())
}

/// Cursor Movement.
fn gd_cursor_move_event(gs: &Rc<RefCell<GtkDisplayScreen>>, event: &gdk::Event) -> Result<()> {
    let mut borrowed_gs = gs.borrow_mut();
    let (width, height) = match &borrowed_gs.cairo_image {
        Some(image) => (image.width() as f64, image.height() as f64),
        None => return Ok(()),
    };

    let (x, y) = match event.coords() {
        Some(value) => value,
        None => return Ok(()),
    };
    let (real_x, real_y) = borrowed_gs.convert_coord(x, y)?;
    let standard_x = ((real_x * (ABS_MAX as f64)) / width) as u16;
    let standard_y = ((real_y * (ABS_MAX as f64)) / height) as u16;

    point_event(
        borrowed_gs.click_state.button_mask as u32,
        standard_x as u32,
        standard_y as u32,
    )
}

fn da_pointer_callback(
    gs: &Rc<RefCell<GtkDisplayScreen>>,
    button_event: &gdk::EventButton,
) -> Result<()> {
    let mut borrowed_gs = gs.borrow_mut();
    borrowed_gs.click_state.button_mask = match button_event.button() {
        1 => INPUT_POINT_LEFT,
        2 => INPUT_POINT_RIGHT,
        3 => INPUT_POINT_MIDDLE,
        _ => return Ok(()),
    };

    let (width, height) = match &borrowed_gs.cairo_image {
        Some(image) => (image.width() as f64, image.height() as f64),
        None => return Ok(()),
    };

    let (x, y) = button_event.position();
    let (real_x, real_y) = borrowed_gs.convert_coord(x, y)?;

    let standard_x = ((real_x * (ABS_MAX as f64)) / width) as u16;
    let standard_y = ((real_y * (ABS_MAX as f64)) / height) as u16;

    match button_event.event_type() {
        gdk::EventType::ButtonRelease => {
            borrowed_gs.click_state.button_mask = 0;
            point_event(
                borrowed_gs.click_state.button_mask as u32,
                standard_x as u32,
                standard_y as u32,
            )
        }
        gdk::EventType::ButtonPress => point_event(
            borrowed_gs.click_state.button_mask as u32,
            standard_x as u32,
            standard_y as u32,
        ),
        gdk::EventType::DoubleButtonPress => press_mouse(
            borrowed_gs.click_state.button_mask as u32,
            standard_x as u32,
            standard_y as u32,
        ),
        _ => Ok(()),
    }
}

fn da_scroll_callback(
    gs: &Rc<RefCell<GtkDisplayScreen>>,
    scroll_event: &gdk::EventScroll,
) -> Result<()> {
    let borrowed_gs = gs.borrow();
    let (width, height) = match &borrowed_gs.cairo_image {
        Some(image) => (image.width() as f64, image.height() as f64),
        None => return Ok(()),
    };
    let button_mask = match scroll_event.direction() {
        ScrollDirection::Up => INPUT_BUTTON_WHEEL_UP,
        ScrollDirection::Down => INPUT_BUTTON_WHEEL_DOWN,
        ScrollDirection::Left => INPUT_BUTTON_WHEEL_LEFT,
        ScrollDirection::Right => INPUT_BUTTON_WHEEL_RIGHT,
        ScrollDirection::Smooth => match scroll_event.scroll_deltas() {
            Some((_, delta_y)) => {
                if delta_y == 0.0 {
                    return Ok(());
                }
                if delta_y > 0.0 {
                    INPUT_BUTTON_WHEEL_DOWN
                } else {
                    INPUT_BUTTON_WHEEL_UP
                }
            }
            None => return Ok(()),
        },
        _ => 0x0,
    };

    let standard_x = ((borrowed_gs.click_state.last_x as u64 * ABS_MAX) / width as u64) as u16;
    let standard_y = ((borrowed_gs.click_state.last_y as u64 * ABS_MAX) / height as u64) as u16;
    drop(borrowed_gs);
    point_event(button_mask, standard_x as u32, standard_y as u32)?;
    Ok(())
}

/// Draw_area callback func for draw signal.
fn da_draw_callback(gs: &Rc<RefCell<GtkDisplayScreen>>, cr: &cairo::Context) -> Result<()> {
    let mut borrowed_gs = gs.borrow_mut();
    let scale_mode = borrowed_gs.scale_mode.clone();
    let (mut surface_width, mut surface_height) = match &borrowed_gs.cairo_image {
        Some(image) => (image.width() as f64, image.height() as f64),
        None => return Ok(()),
    };

    if surface_width.le(&0.0) || surface_height.le(&0.0) {
        return Ok(());
    }

    let (window_width, window_height);
    match borrowed_gs.get_window_size() {
        Some((w, h)) => (window_width, window_height) = (w, h),
        None => return Ok(()),
    };

    if scale_mode.borrow().is_full_screen() || scale_mode.borrow().is_free_scale() {
        borrowed_gs.scale_x = window_width / surface_width;
        borrowed_gs.scale_y = window_height / surface_height;
    }
    surface_width *= borrowed_gs.scale_x;
    surface_height *= borrowed_gs.scale_y;

    let mut mx: f64 = 0.0;
    let mut my: f64 = 0.0;
    if window_width.gt(&surface_width) {
        mx = (window_width - surface_width) / (2.0);
    }
    if window_height.gt(&surface_height) {
        my = (window_height - surface_height) / (2.0);
    }

    cr.rectangle(0.0, 0.0, window_width, window_height);
    cr.rectangle(mx + surface_width, my, surface_width * -1.0, surface_height);
    cr.fill()?;
    cr.scale(borrowed_gs.scale_x, borrowed_gs.scale_y);
    if let Some(image) = &borrowed_gs.cairo_image {
        cr.set_source_surface(image, mx / borrowed_gs.scale_x, my / borrowed_gs.scale_y)?;
    }
    cr.paint()?;

    Ok(())
}

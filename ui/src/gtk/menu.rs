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

use anyhow::{bail, Result};
use gtk::{
    gdk::{
        self,
        ffi::{GDK_KEY_equal, GDK_KEY_minus, GDK_KEY_B, GDK_KEY_F, GDK_KEY_M, GDK_KEY_S},
        ModifierType,
    },
    glib,
    prelude::{AccelGroupExtManual, NotebookExtManual},
    traits::{
        BoxExt, CheckMenuItemExt, ContainerExt, GtkMenuExt, GtkMenuItemExt, GtkWindowExt,
        MenuShellExt, NotebookExt, WidgetExt,
    },
    AccelFlags, AccelGroup, ApplicationWindow, CheckMenuItem, Inhibit, Menu, MenuBar, MenuItem,
    Orientation, RadioMenuItem,
};
use log::error;

use crate::gtk::{
    renew_image, update_window_size, GtkDisplay, ZoomOperate, GTK_SCALE_MIN, GTK_ZOOM_STEP,
};

#[derive(Clone)]
pub(crate) struct GtkMenu {
    pub(crate) window: ApplicationWindow,
    container: gtk::Box,
    pub(crate) note_book: gtk::Notebook,
    pub(crate) radio_group: Vec<RadioMenuItem>,
    accel_group: AccelGroup,
    menu_bar: MenuBar,
    machine_menu: Menu,
    machine_item: MenuItem,
    shutdown_item: MenuItem,
    pub(crate) view_menu: Menu,
    view_item: MenuItem,
    full_screen_item: MenuItem,
    zoom_in_item: MenuItem,
    zoom_out_item: MenuItem,
    zoom_fit: CheckMenuItem,
    best_fit_item: MenuItem,
    show_menu_bar: CheckMenuItem,
}

impl GtkMenu {
    pub(crate) fn new(window: ApplicationWindow) -> Self {
        Self {
            window,
            container: gtk::Box::new(Orientation::Vertical, 0),
            note_book: gtk::Notebook::default(),
            radio_group: vec![],
            accel_group: AccelGroup::default(),
            menu_bar: MenuBar::new(),
            machine_menu: Menu::new(),
            machine_item: MenuItem::with_label("Machine"),
            shutdown_item: MenuItem::with_label("Shut Down"),
            view_menu: Menu::new(),
            view_item: MenuItem::with_label("View"),
            full_screen_item: MenuItem::with_label("Full Screen"),
            zoom_in_item: MenuItem::with_label("Zoom In"),
            zoom_out_item: MenuItem::with_label("Zoom Out"),
            zoom_fit: CheckMenuItem::with_label("Zoom Fit"),
            best_fit_item: MenuItem::with_label("Best Fit"),
            show_menu_bar: CheckMenuItem::with_label("Show MenuBar"),
        }
    }

    /// 1. Setting callback function for button.
    /// 2. Set shortcut keys for buttons.
    /// Button                  shortcut key
    /// shutdown_item:          Ctrl + Alt + S.
    /// full_screen_item        Ctrl + Alt + F
    /// zoom_in_item            Ctrl + Alt + +
    /// zoom_out_item           Ctrl + Alt + -
    /// best_fit_item           Ctrl + Alt + B
    /// show_menu_bar           Ctrl + Alt + M
    pub(crate) fn set_signal(&mut self, gd: &Rc<RefCell<GtkDisplay>>) {
        let modifier = ModifierType::CONTROL_MASK | ModifierType::MOD1_MASK;
        let accel_flags = AccelFlags::VISIBLE;

        self.shutdown_item
            .connect_activate(glib::clone!(@weak gd => move |_| {
                power_down_callback(&gd).unwrap_or_else(|e| error!("Gtk shutdown failed: {:?}", e));
            }));
        self.shutdown_item.add_accelerator(
            "activate",
            &self.accel_group,
            GDK_KEY_S as u32,
            modifier,
            accel_flags,
        );

        self.full_screen_item
            .connect_activate(glib::clone!(@weak gd => move |_| {
                full_screen_callback(&gd).unwrap_or_else(|e| error!("Full Screen Item: {:?}", e));
            }));
        self.full_screen_item.add_accelerator(
            "activate",
            &self.accel_group,
            GDK_KEY_F as u32,
            modifier,
            accel_flags,
        );
        let full_screen_item = self.full_screen_item.clone();
        self.accel_group.connect_accel_group(
            GDK_KEY_F as u32,
            modifier,
            accel_flags,
            glib::clone!(@weak full_screen_item => @default-return false, move |_, _, _, _| {
                full_screen_item.activate();
                false
            }),
        );

        self.zoom_in_item
            .connect_activate(glib::clone!(@weak gd => move |_| {
                menu_zoom_callback(&gd, ZoomOperate::ZoomIn).unwrap_or_else(|e| error!("Zoom In Item: {:?}", e));
            }));
        self.zoom_in_item.add_accelerator(
            "activate",
            &self.accel_group,
            GDK_KEY_equal as u32,
            modifier,
            accel_flags,
        );

        self.zoom_out_item
            .connect_activate(glib::clone!(@weak gd => move |_| {
                menu_zoom_callback(&gd, ZoomOperate::ZoomOut).unwrap_or_else(|e| error!("Zoom Out Item: {:?}", e));
            }));
        self.zoom_out_item.add_accelerator(
            "activate",
            &self.accel_group,
            GDK_KEY_minus as u32,
            modifier,
            accel_flags,
        );

        self.best_fit_item
            .connect_activate(glib::clone!(@weak gd => move |_| {
                menu_zoom_callback(&gd, ZoomOperate::BestFit).unwrap_or_else(|e| error!("Best Fit Item: {:?}", e));
            }));
        self.best_fit_item.add_accelerator(
            "activate",
            &self.accel_group,
            GDK_KEY_B as u32,
            modifier,
            accel_flags,
        );

        // Set the hiding of menu_bar.
        self.show_menu_bar
            .connect_activate(glib::clone!(@weak gd => move |_| {
                show_menubar_callback(&gd).unwrap_or_else(|e| error!("Shoe Menu Bar: {:?}", e));
            }));
        let show_menu_bar = self.show_menu_bar.clone();
        self.show_menu_bar.add_accelerator(
            "activate",
            &self.accel_group,
            GDK_KEY_M as u32,
            modifier,
            accel_flags,
        );
        self.accel_group.connect_accel_group(
            GDK_KEY_M as u32,
            modifier,
            accel_flags,
            move |_, _, _, _| {
                if !show_menu_bar.is_active() {
                    show_menu_bar.activate();
                }
                true
            },
        );

        // Connect delete for window.
        self.window.connect_delete_event(
            glib::clone!(@weak gd => @default-return Inhibit(false), move |_, _| {
                power_down_callback(&gd).unwrap_or_else(|e| error!("Standard vm write power button failed: {:?}", e));
                Inhibit(false)
            }),
        );

        // By confirmation this button, the size of window is fixed and
        // can not be changed.
        self.zoom_fit
            .connect_activate(glib::clone!(@weak gd => move |_| {
                zoom_fit_callback(&gd).unwrap_or_else(|e| error!("Zoom fit: {:?}", e));
            }));
    }

    pub(crate) fn set_menu(&mut self) {
        // Machine menu.
        self.machine_menu.set_accel_group(Some(&self.accel_group));
        self.machine_menu.append(&self.shutdown_item);
        self.machine_item.set_submenu(Some(&self.machine_menu));

        // View menu.
        self.view_menu.set_accel_group(Some(&self.accel_group));
        self.view_menu.append(&self.full_screen_item);
        self.view_menu.append(&self.zoom_in_item);
        self.view_menu.append(&self.zoom_out_item);
        self.view_menu.append(&self.zoom_fit);
        self.view_menu.append(&self.best_fit_item);
        self.view_menu.append(&self.show_menu_bar);
        self.view_item.set_submenu(Some(&self.view_menu));

        self.menu_bar.append(&self.machine_item);
        self.menu_bar.append(&self.view_item);

        // Set the visible of note_book.
        self.note_book.set_show_tabs(false);
        self.note_book.set_show_border(true);

        self.window.add_accel_group(&self.accel_group);
        self.container.pack_start(&self.menu_bar, false, false, 0);
        self.container.pack_start(&self.note_book, true, true, 0);
        self.window.add(&self.container);
    }

    /// Show window.
    pub(crate) fn show_window(&self, is_full_screen: bool) {
        if is_full_screen {
            self.menu_bar.hide();
            self.window.fullscreen();
        }
        self.window.show_all();
    }
}

/// Fixed the window size.
fn power_down_callback(gd: &Rc<RefCell<GtkDisplay>>) -> Result<()> {
    let power_button = gd.borrow().power_button.clone();
    power_button.write(1)?;
    Ok(())
}

/// Hid/show title bar.
fn show_menubar_callback(gd: &Rc<RefCell<GtkDisplay>>) -> Result<()> {
    let borrowed_gd = gd.borrow();
    let gtk_menu = borrowed_gd.gtk_menu.clone();
    if borrowed_gd.scale_mode.borrow().is_full_screen() {
        return Ok(());
    }
    if gtk_menu.show_menu_bar.is_active() {
        gtk_menu.menu_bar.show();
    } else {
        gtk_menu.menu_bar.hide();
    }
    drop(gtk_menu);

    let active_gs = borrowed_gd.get_current_display()?;
    drop(borrowed_gd);
    update_window_size(&active_gs)
}

/// Make the window to fill the entir desktop.
fn full_screen_callback(gd: &Rc<RefCell<GtkDisplay>>) -> Result<()> {
    let borrowed_gd = gd.borrow();
    let gtk_menu = borrowed_gd.gtk_menu.clone();
    let gs = borrowed_gd.get_current_display()?;
    let scale_mode = borrowed_gd.scale_mode.clone();
    let mut borrowed_scale = scale_mode.borrow_mut();
    drop(borrowed_gd);
    if !borrowed_scale.is_full_screen() {
        gtk_menu.note_book.set_show_tabs(false);
        gtk_menu.menu_bar.hide();
        gs.borrow().draw_area.set_size_request(-1, -1);
        if let Some(screen) = gdk::Screen::default() {
            let width = screen.width();
            let height = screen.height();
            gs.borrow().window.set_default_size(width, height);
        }
        gtk_menu.window.fullscreen();
        borrowed_scale.full_screen = true;
    } else {
        gtk_menu.window.unfullscreen();
        if gtk_menu.show_menu_bar.is_active() {
            gtk_menu.menu_bar.show();
        }
        borrowed_scale.full_screen = false;
        gs.borrow_mut().scale_x = 1.0;
        gs.borrow_mut().scale_y = 1.0;
        drop(borrowed_scale);
        update_window_size(&gs)?;
    };

    Ok(())
}

/// Zoom in/out the display.
fn menu_zoom_callback(gd: &Rc<RefCell<GtkDisplay>>, zoom_opt: ZoomOperate) -> Result<()> {
    let borrowed_gd = gd.borrow();
    let page_num = borrowed_gd.gtk_menu.note_book.current_page();
    let gs = match borrowed_gd.get_ds_by_pagenum(page_num) {
        Some(ds) => ds,
        None => bail!("Display Can not found."),
    };
    drop(borrowed_gd);
    let mut borrowed_gs = gs.borrow_mut();
    match zoom_opt {
        ZoomOperate::ZoomIn => {
            borrowed_gs.scale_x += GTK_ZOOM_STEP;
            borrowed_gs.scale_y += GTK_ZOOM_STEP;
        }
        ZoomOperate::ZoomOut => {
            borrowed_gs.scale_x -= GTK_ZOOM_STEP;
            borrowed_gs.scale_y -= GTK_ZOOM_STEP;
            borrowed_gs.scale_x = borrowed_gs.scale_x.max(GTK_SCALE_MIN);
            borrowed_gs.scale_y = borrowed_gs.scale_y.max(GTK_SCALE_MIN);
        }
        ZoomOperate::BestFit => {
            borrowed_gs.scale_x = 1.0;
            borrowed_gs.scale_y = 1.0;
        }
    }
    drop(borrowed_gs);
    update_window_size(&gs)
}

/// Fixed the window size.
fn zoom_fit_callback(gd: &Rc<RefCell<GtkDisplay>>) -> Result<()> {
    let gtk_menu = gd.borrow().gtk_menu.clone();
    let gs = gd.borrow().get_current_display()?;
    if gtk_menu.zoom_fit.is_active() {
        gd.borrow_mut().scale_mode.borrow_mut().free_scale = true;
    } else {
        gd.borrow_mut().scale_mode.borrow_mut().free_scale = false;
        gs.borrow_mut().scale_x = 1.0;
        gs.borrow_mut().scale_y = 1.0;
    }

    update_window_size(&gs)?;
    renew_image(&gs)
}

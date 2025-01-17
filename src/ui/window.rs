// Furtherance - Track your time without being tracked
// Copyright (C) 2022  Ricky Kresslein <rk@lakoliu.com>
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use adw::subclass::prelude::AdwApplicationWindowImpl;
use gettextrs::*;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gio, glib, CompositeTemplate};
use glib::{clone, timeout_add_local};
use std::time::Duration;
use std::sync::Mutex;
use std::rc::Rc;
use std::cell::RefCell;
use chrono::{DateTime, Local, Duration as ChronDur};
use dbus::blocking::Connection;

use crate::ui::FurHistoryBox;
use crate::FurtheranceApplication;
use crate::database;
use crate::settings_manager;
use crate::config;

mod imp {
    use super::*;

    #[derive(Debug, Default, CompositeTemplate)]
    #[template(resource = "/com/lakoliu/Furtherance/gtk/window.ui")]
    pub struct FurtheranceWindow {
        // Template widgets
        #[template_child]
        pub header_bar: TemplateChild<adw::HeaderBar>,
        #[template_child]
        pub watch: TemplateChild<gtk::Label>,
        #[template_child]
        pub task_input: TemplateChild<gtk::Entry>,
        #[template_child]
        pub start_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub history_box: TemplateChild<FurHistoryBox>,
        #[template_child]
        pub toast_overlay: TemplateChild<adw::ToastOverlay>,

        pub stored_idle: Mutex<u64>,
        pub idle_notified: Mutex<bool>,
        pub idle_time_reached: Mutex<bool>,
        pub subtract_idle: Mutex<bool>,
        pub idle_start_time: Mutex<String>,
        pub running: Mutex<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FurtheranceWindow {
        const NAME: &'static str = "FurtheranceWindow";
        type Type = super::FurtheranceWindow;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            FurHistoryBox::static_type();
            Self::bind_template(klass);
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for FurtheranceWindow {
        fn constructed(&self, obj: &Self::Type) {
            obj.setup_widgets();
            obj.setup_signals();
            obj.setup_settings();
            self.parent_constructed(obj);
        }
    }
    impl WidgetImpl for FurtheranceWindow {}
    impl WindowImpl for FurtheranceWindow {}
    impl ApplicationWindowImpl for FurtheranceWindow {}
    impl AdwApplicationWindowImpl for FurtheranceWindow {}
}

glib::wrapper! {
    pub struct FurtheranceWindow(ObjectSubclass<imp::FurtheranceWindow>)
        @extends gtk::Widget, gtk::Window, gtk::ApplicationWindow,
        @implements gio::ActionGroup, gio::ActionMap;
}

impl FurtheranceWindow {
    pub fn new<P: glib::IsA<gtk::Application>>(application: &P) -> Self {
        glib::Object::new(&[("application", application)])
            .expect("Failed to create FurtheranceWindow")
    }

    pub fn display_toast(&self, text: &str) {
        // Display in-app notifications
        let imp = imp::FurtheranceWindow::from_instance(self);
        let toast = adw::Toast::new(text);
        imp.toast_overlay.add_toast(&toast);
    }

    fn set_watch_time(&self, text: &str) {
        // Update watch time while timer is running
        let imp = imp::FurtheranceWindow::from_instance(self);
        imp.watch.set_text(text);
        if settings_manager::get_bool("notify-of-idle") {
            self.check_user_idle();
        }
    }

    fn activate_task_input(&self, sensitive: bool) {
        // Deactivate task_input while timer is running
        let imp = imp::FurtheranceWindow::from_instance(self);
        imp.task_input.set_sensitive(sensitive);
    }

    fn get_task_text(&self) -> String {
        let imp = imp::FurtheranceWindow::from_instance(self);
        imp.task_input.text().to_string()
    }

    fn save_task(&self, start_time: DateTime<Local>, mut stop_time: DateTime<Local>) {
        // Save the most recent task to the database and clear the task_input field
        let imp = imp::FurtheranceWindow::from_instance(self);

        if *imp.subtract_idle.lock().unwrap() {
            let idle_start = DateTime::parse_from_rfc3339(&imp.idle_start_time.lock().unwrap()).unwrap();
            stop_time = idle_start.with_timezone(&Local);
            *imp.subtract_idle.lock().unwrap() = false;
        }

        let _ = database::db_write(&imp.task_input.text().trim(), start_time, stop_time);
        imp.task_input.set_text("");
        imp.history_box.create_tasks_page();
    }

    pub fn reset_history_box(&self) {
        let imp = imp::FurtheranceWindow::from_instance(self);
        imp.history_box.create_tasks_page();
    }

    fn setup_widgets(&self) {
        let imp = imp::FurtheranceWindow::from_instance(self);

        // Set initial minimum height
        let is_saved_task: bool = match database::check_for_tasks() {
            Ok(_) => true,
            Err(_) => false,
        };
        if is_saved_task {
            self.set_height_request(300);
        } else {
            self.set_height_request(390);
        }

        // Development mode
        if config::PROFILE == "development" {
            self.add_css_class("devel");
        }

        imp.task_input.grab_focus();
    }

    fn setup_signals(&self) {
        let imp = imp::FurtheranceWindow::from_instance(self);
        // running = false
        *imp.running.lock().unwrap() = false;
        let start_time = Rc::new(RefCell::new(Local::now()));
        let stop_time = Rc::new(RefCell::new(Local::now()));

        imp.start_button.connect_clicked(clone!(
            @weak self as this => move |button| {
            let imp2 = imp::FurtheranceWindow::from_instance(&this);
            if this.get_task_text().trim().is_empty() {
                let dialog = gtk::MessageDialog::with_markup(
                    Some(&this),
                    gtk::DialogFlags::MODAL,
                    gtk::MessageType::Error,
                    gtk::ButtonsType::Ok,
                    Some(&format!("<span size='large'>{}</span>", &gettext("No Task Name"))),
                );
                dialog.set_secondary_text(Some(&gettext("Enter a task name to start the timer.")));
                dialog.show();

                dialog.connect_response(clone!(@strong dialog => move |_,_|{
                    dialog.close();
                }));

            } else {
                if !*imp2.running.lock().unwrap() {
                    let mut secs: u32 = 0;
                    let mut mins: u32 = 0;
                    let mut hrs: u32 = 0;

                    *imp2.running.lock().unwrap() = true;
                    *start_time.borrow_mut() = Local::now();
                    this.activate_task_input(false);
                    let duration = Duration::new(1,0);
                    timeout_add_local(duration, clone!(@strong this as this_clone => move || {
                        let imp3 = imp::FurtheranceWindow::from_instance(&this_clone);
                        if *imp3.running.lock().unwrap() {
                            secs += 1;
                            if secs > 59 {
                                secs = 0;
                                mins += 1;
                                if mins > 59 {
                                    mins = 0;
                                    hrs += 1;
                                }
                            }
                            let watch_text: &str = &format!("{:02}:{:02}:{:02}", hrs, mins, secs).to_string();
                            this_clone.set_watch_time(watch_text);
                        }
                        Continue(*imp3.running.lock().unwrap())
                    }));
                    button.set_icon_name("media-playback-stop-symbolic");
                } else {
                    *stop_time.borrow_mut() = Local::now();
                    *imp2.running.lock().unwrap() = false;
                    button.set_icon_name("media-playback-start-symbolic");
                    this.set_watch_time("00:00:00");
                    this.activate_task_input(true);
                    this.save_task(*start_time.borrow(), *stop_time.borrow());
                }
            }
        }));
    }

    fn setup_settings(&self) {
        let imp = imp::FurtheranceWindow::from_instance(self);
        self.reset_vars();

        // Enter starts timer
        let start = imp.start_button.clone();
        self.set_default_widget(Some(&start));
        imp.task_input.set_activates_default(true);
    }

    fn get_idle_time(&self) -> Result<u64, Box<dyn std::error::Error>> {
        let c = Connection::new_session()?;

        let p = c.with_proxy("org.gnome.Mutter.IdleMonitor",
            "/org/gnome/Mutter/IdleMonitor/Core",
            Duration::from_millis(5000)
        );
        let (idle_time,): (u64,) = p.method_call("org.gnome.Mutter.IdleMonitor", "GetIdletime", ())?;

        Ok(idle_time / 1000)
    }

    fn check_user_idle(&self) {
        let imp = imp::FurtheranceWindow::from_instance(self);
        // Check for user idle
        let idle_time = match self.get_idle_time() {
            Ok(val) => val,
            Err(_) => 1,
        };

        // If user was idle and has now returned...
        if idle_time < (settings_manager::get_int("idle-time") * 60) as u64
            && *imp.idle_time_reached.lock().unwrap()
            && !*imp.idle_notified.lock().unwrap() {

                *imp.idle_notified.lock().unwrap() = true;
                self.resume_from_idle();
        }
        *imp.stored_idle.lock().unwrap() = idle_time;

        // If user is idle but has not returned...
        if *imp.stored_idle.lock().unwrap() >= (settings_manager::get_int("idle-time") * 60) as u64
            && !*imp.idle_time_reached.lock().unwrap() {

            *imp.idle_time_reached.lock().unwrap() = true;
            let true_idle_start_time = Local::now() -
                ChronDur::seconds((settings_manager::get_int("idle-time") * 60) as i64);
            *imp.idle_start_time.lock().unwrap() = true_idle_start_time.to_rfc3339();
        }
    }

    fn resume_from_idle(&self) {
        let imp = imp::FurtheranceWindow::from_instance(self);

        let resume_time = Local::now();
        let idle_start = DateTime::parse_from_rfc3339(&imp.idle_start_time.lock().unwrap()).unwrap();
        let idle_start = idle_start.with_timezone(&Local);
        let idle_time = resume_time - idle_start;
        let idle_time = idle_time.num_seconds();
        let h = idle_time / 60 / 60;
        let m = (idle_time / 60) - (h * 60);
        let s = idle_time - (m * 60);
        let idle_time_str = format!("{}{:02}:{:02}:{:02}", gettext("You have been idle for "), h, m, s);
        let question_str = gettext("\nWould you like to discard that time, or continue the clock?");
        let idle_time_msg = format!("{}{}", idle_time_str, question_str);

        let dialog = gtk::MessageDialog::with_markup(
            Some(self),
            gtk::DialogFlags::MODAL,
            gtk::MessageType::Warning,
            gtk::ButtonsType::None,
            Some(&format!("<span size='x-large' weight='bold'>{}</span>", &gettext("Edit Task"))),
        );
        dialog.add_buttons(&[
            (&gettext("Discard"), gtk::ResponseType::Reject),
            (&gettext("Continue"), gtk::ResponseType::Accept)
        ]);
        dialog.set_secondary_text(Some(&idle_time_msg));

        let app = FurtheranceApplication::default();
        app.system_notification(&idle_time_str, &question_str, dialog.clone());

        dialog.connect_response(clone!(
            @weak self as this,
            @strong dialog,
            @strong imp.start_button as start_button => move |_, resp| {
            if resp == gtk::ResponseType::Reject {
                this.set_subtract_idle(true);
                start_button.emit_clicked();
                dialog.close();
            } else {
                this.reset_vars();
                dialog.close();
            }
        }));

        dialog.show()
    }

    pub fn reset_vars(&self) {
        let imp = imp::FurtheranceWindow::from_instance(self);
        *imp.stored_idle.lock().unwrap() = 0;
        *imp.idle_notified.lock().unwrap() = false;
        *imp.idle_time_reached.lock().unwrap() = false;
        *imp.subtract_idle.lock().unwrap() = false;
    }

    pub fn set_subtract_idle(&self, val: bool) {
        let imp = imp::FurtheranceWindow::from_instance(self);
        *imp.subtract_idle.lock().unwrap() = val;
    }

    pub fn duplicate_task(&self, task_name_text: String) {
        let imp = imp::FurtheranceWindow::from_instance(self);
        if !*imp.running.lock().unwrap() {
            imp.task_input.set_text(&task_name_text);
            imp.start_button.emit_clicked();
        } else {
            self.display_toast("Stop the timer to duplicate a task.");
        }
    }
}

impl Default for FurtheranceWindow {
    fn default() -> Self {
        FurtheranceApplication::default()
            .active_window()
            .unwrap()
            .downcast()
            .unwrap()
    }
}

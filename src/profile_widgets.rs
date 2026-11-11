use crate::services::ProfileId;
use gtk::{gio, glib, prelude::*};

fn empty_row() -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.set_hexpand(true);
    let icon = gtk::Image::from_icon_name("view-grid-symbolic");
    icon.add_css_class("dim-label");
    icon.set_tooltip_text(Some("Predefined profile"));
    icon.update_property(&[gtk::accessible::Property::Label("Predefined profile")]);
    let label = gtk::Label::new(None);
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_max_width_chars(32);
    row.append(&label);
    row.append(&icon);
    row
}

fn update_row(row: &gtk::Box, profile: &ProfileId) {
    let label = row.first_child().unwrap().downcast::<gtk::Label>().unwrap();
    let icon = row.last_child().unwrap();
    let predefined = matches!(profile, ProfileId::Predefined { .. });
    icon.set_visible(predefined);
    label.set_text(profile.name());
    row.set_tooltip_text(Some(if predefined {
        "Predefined profile"
    } else {
        profile.name()
    }));
    row.update_property(&[gtk::accessible::Property::Label(&profile.label())]);
}

pub(crate) fn row(profile: &ProfileId) -> gtk::Box {
    let row = empty_row();
    update_row(&row, profile);
    row
}

pub(crate) fn model(profiles: &[ProfileId]) -> gio::ListStore {
    let model = gio::ListStore::new::<glib::BoxedAnyObject>();
    for profile in profiles {
        model.append(&glib::BoxedAnyObject::new(profile.clone()));
    }
    model
}

fn factory(show_selection: bool) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let row = empty_row();
        if show_selection {
            let check = gtk::Image::from_icon_name("object-select-symbolic");
            item.bind_property("selected", &check, "visible")
                .sync_create()
                .build();
            row.insert_child_after(&check, row.first_child().as_ref());
        }
        item.set_child(Some(&row));
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let value = item
            .item()
            .unwrap()
            .downcast::<glib::BoxedAnyObject>()
            .unwrap();
        let profile = value.borrow::<ProfileId>();
        let row = item.child().unwrap().downcast::<gtk::Box>().unwrap();
        update_row(&row, &profile);
    });
    factory
}

pub(crate) fn configure_dropdown(dropdown: &gtk::DropDown) {
    dropdown.set_factory(Some(&factory(false)));
    dropdown.set_list_factory(Some(&factory(true)));
}

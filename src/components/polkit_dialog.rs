// If the way this handles surface/window is awkward, could inform design of multi-window in iced

#![allow(clippy::single_match)]

use crate::fl;
use crate::icons::{self, lucide_icon};
use crate::subscriptions::polkit_agent::PolkitError;
use crate::subscriptions::polkit_agent_helper;
use cosmic::iced::event::{PlatformSpecific, wayland};
use cosmic::iced::platform_specific::shell::commands::layer_surface::{
    KeyboardInteractivity, Layer, destroy_layer_surface,
};
use cosmic::iced::runtime::platform_specific::wayland::layer_surface::SctkLayerSurfaceSettings;
use cosmic::iced::window::Id as SurfaceId;
use cosmic::iced::{self, Subscription, Task};
use cosmic::surface::action::{LiveSettings, simple_layer_shell};
use cosmic::{Element, widget};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use tokio::sync::oneshot;

pub static POLKIT_DIALOG_ID: LazyLock<widget::Id> =
    LazyLock::new(|| widget::Id::new("polkit-dialog".to_string()));

/// Channel the dialog answers on. Wrapped in `Arc<Mutex<Option<..>>>` because the sender
/// is single-use but the params are cloned across the surface lifecycle.
pub type ResponseSender = Arc<Mutex<Option<oneshot::Sender<Result<(), PolkitError>>>>>;

#[derive(Clone, Debug)]
pub struct Params {
    pub pw_name: String,
    /// Part of the polkit `BeginAuthentication` payload. Retained so this struct stays
    /// faithful to the wire format and shows up in `Debug` output when diagnosing
    /// authentication failures, even though the dialog does not render it.
    #[allow(dead_code)]
    pub action_id: String,
    pub message: String,
    pub icon_name: Option<String>,
    /// As `action_id`: part of the payload, kept for fidelity and diagnostics.
    #[allow(dead_code)]
    pub details: HashMap<String, String>,
    pub cookie: String,
    // XXX `Clone` bound is awkward here
    pub response_sender: ResponseSender,
}

#[derive(Clone, Debug)]
pub enum Msg {
    Agent(polkit_agent_helper::Event),
    Authenticate,
    Cancel,
    Layer(wayland::LayerEvent),
    Password(String),
    Sent(bool),
    TogglePasswordVisibility,
}

pub struct State {
    id: SurfaceId,
    pub params: Params,
    responder: Option<polkit_agent_helper::Responder>,
    password: String,
    password_visible: bool,
    message: Option<String>, // TODO show
    password_label: String,  // TODO
    echo: bool,
    pub text_input_id: iced::id::Id,
    sensitive: bool,
    retries: u32,
    // TODO: Better way to use fluent with iced?
    msg_cancel: String,
    msg_authenticate: String,
    msg_authentication_required: String,
    msg_invalid_password: String,
}

impl State {
    pub fn new<T: Send + Sync + 'static>(
        id: SurfaceId,
        params: Params,
    ) -> (Self, Task<cosmic::Action<T>>) {
        let text_input_id = iced::id::Id::unique();
        let cmd = cosmic::surface::surface_task(simple_layer_shell(
            LiveSettings::default,
            move || SctkLayerSurfaceSettings {
                id,
                keyboard_interactivity: KeyboardInteractivity::Exclusive,
                namespace: "osd".into(),
                layer: Layer::Overlay,
                size: None,
                ..Default::default()
            },
            None::<fn() -> Element<'static, cosmic::Action<Msg>>>,
        ));
        (
            Self {
                id,
                params,
                responder: None,
                password: String::new(),
                password_visible: false,
                message: None,
                password_label: String::new(),
                echo: false,
                text_input_id,
                sensitive: true,
                retries: 0,
                msg_cancel: fl!("cancel"),
                msg_authenticate: fl!("authenticate"),
                msg_authentication_required: fl!("authentication-required"),
                msg_invalid_password: fl!("invalid-password"),
            },
            cmd,
        )
    }

    pub fn cancel<T>(self) -> Task<T> {
        self.respond(Err(PolkitError::Cancelled))
    }

    fn respond<T>(self, res: Result<(), PolkitError>) -> Task<T> {
        let sender = self.params.response_sender.lock().unwrap().take().unwrap();
        let _ = sender.send(res);
        destroy_layer_surface(self.id)
    }

    pub fn update(mut self, event: Msg) -> (Option<Self>, Task<Msg>) {
        match event {
            // XXX which layer?
            Msg::Layer(layer_event) => match layer_event {
                wayland::LayerEvent::Focused => {
                    let cmd = widget::text_input::focus(self.text_input_id.clone());
                    return (Some(self), cmd);
                }
                _ => {}
            },
            Msg::Agent(agent_msg) => match agent_msg {
                polkit_agent_helper::Event::Responder(responder) => {
                    self.responder = Some(responder);
                }
                polkit_agent_helper::Event::Failed => {
                    return (None, self.respond(Err(PolkitError::Failed)));
                }
                polkit_agent_helper::Event::Request(s, echo) => {
                    self.password_label = s;
                    self.echo = echo;
                }
                polkit_agent_helper::Event::ShowError(s) => {
                    self.message = Some(s);
                }
                polkit_agent_helper::Event::ShowDebug(s) => {
                    self.message = Some(s);
                }
                polkit_agent_helper::Event::Complete(success) => {
                    if success {
                        return (None, self.respond(Ok(())));
                    } else {
                        self.retries += 1;
                        self.sensitive = true;
                        self.responder = None;
                        self.password.clear();
                        let cmd = widget::text_input::focus(self.text_input_id.clone());
                        return (Some(self), cmd);
                    };
                }
            },
            Msg::Authenticate => {
                self.sensitive = false; // TODO: show spinner?
                if let Some(responder) = self.responder.clone() {
                    let password = self.password.clone();

                    return (
                        Some(self),
                        Task::perform(
                            async move { responder.response(&password).await },
                            |result| Msg::Sent(result.is_ok()),
                        ),
                    );
                }
            }
            Msg::Cancel => return (None, self.cancel()),
            Msg::Password(password) => {
                self.password = password;
            }
            Msg::TogglePasswordVisibility => {
                self.password_visible = !self.password_visible;
            }
            Msg::Sent(success) => {
                if !success {
                    self.sensitive = true;
                    self.password.clear();

                    log::error!("failed to send password");
                }
            }
        }
        (Some(self), Task::none())
    }

    pub fn view(&self) -> cosmic::Element<'_, Msg> {
        // TODO Allocates on every keypress?

        let placeholder = self.password_label.trim_end_matches(':');
        let mut password_input = if !self.echo {
            // Inlined `widget::secure_input`, whose leading/trailing icons come from
            // the icon theme. Same padding, style, sizes and message wiring; only the
            // glyphs differ, and the toggle now reads as the action it performs
            // (eye = reveal, eye-off = conceal) rather than libcosmic's mapping.
            let spacing = cosmic::theme::active().cosmic().space_xxs();
            let mut input = widget::TextInput::new(placeholder, &self.password)
                .id(self.text_input_id.clone())
                .padding([0, spacing])
                .style(cosmic::theme::TextInput::Default)
                .leading_icon(
                    widget::container(lucide_icon(icons::LOCK, 16))
                        .padding(8)
                        .into(),
                );
            if !self.password_visible {
                input = input.password();
            }
            input.trailing_icon(
                widget::button::custom(lucide_icon(
                    if self.password_visible {
                        icons::EYE_OFF
                    } else {
                        icons::EYE
                    },
                    16,
                ))
                .class(cosmic::theme::Button::Icon)
                .on_press(Msg::TogglePasswordVisibility)
                .padding(8)
                .into(),
            )
        } else {
            widget::text_input(placeholder, &self.password).id(self.text_input_id.clone())
        };
        let mut cancel_button = widget::button::standard(&self.msg_cancel);
        let mut authenticate_button = widget::button::suggested(&self.msg_authenticate);
        if self.sensitive {
            password_input = password_input.on_input(Msg::Password);
            cancel_button = cancel_button.on_press(Msg::Cancel);

            if self.responder.is_some() {
                password_input = password_input.on_submit(|_| Msg::Authenticate);
                authenticate_button = authenticate_button.on_press(Msg::Authenticate);
            }
        }
        let mut right_column: Vec<cosmic::Element<_>> = vec![password_input.into()];
        if self.retries > 0 {
            right_column.push(
                widget::text::body(&self.msg_invalid_password)
                    .class(cosmic::theme::Text::Color(iced::Color::from_rgb(
                        1.0, 0.0, 0.0,
                    )))
                    .into(),
            );
        } else {
            right_column.push(widget::text::body("").into())
        }
        // The polkit action supplies its own identity icon, so that path stays
        // resolved through the icon theme; only our own fallback is Lucide.
        // A name the theme doesn't ship resolves to an empty handle in libcosmic,
        // so check the lookup and take the fallback rather than draw a 64px hole.
        let named = self
            .params
            .icon_name
            .as_deref()
            // `fallback(None)` disables libcosmic's prefix-truncation chain, which
            // would resolve "drive-harddisk-usb" to "drive" and pass this filter
            // with a glyph that is not the action's icon at all.
            .map(|name| widget::icon::from_name(name).fallback(None).size(64))
            .filter(|named| named.clone().path().is_some());
        let icon: cosmic::Element<_> = match named {
            Some(named) => named.into(),
            None => lucide_icon(icons::LOCK_KEYHOLE, 64).into(),
        };
        widget::autosize::autosize(
            widget::dialog::dialog()
                .title(&self.msg_authentication_required)
                .body(&self.params.message)
                .control(widget::column::with_children(right_column).spacing(4))
                .icon(icon)
                .primary_action(authenticate_button)
                .secondary_action(cancel_button),
            POLKIT_DIALOG_ID.clone(),
        )
        .min_width(1.)
        .min_height(1.)
        .into()
    }

    pub fn subscription(&self) -> Subscription<Msg> {
        Subscription::batch([
            iced::event::listen_with(|e, _status, _id| match e {
                iced::Event::PlatformSpecific(PlatformSpecific::Wayland(
                    wayland::Event::Layer(e, ..),
                )) => Some(Msg::Layer(e)),
                _ => None,
            }),
            polkit_agent_helper::subscription(&self.params.pw_name, &self.params.cookie)
                .map(Msg::Agent),
        ])
    }
}

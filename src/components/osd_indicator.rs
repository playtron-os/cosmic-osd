// TODO: Dismiss on click?

use crate::components::app::DisplayMode;
use crate::config;
use crate::icons::lucide_icon;
use cosmic::iced::platform_specific::shell::commands::layer_surface::{
    Anchor, KeyboardInteractivity, Layer, destroy_layer_surface,
};
use cosmic::iced::runtime::platform_specific::wayland::layer_surface::{
    IcedMargin, IcedOutput, SctkLayerSurfaceSettings,
};
use cosmic::iced::window::Id as SurfaceId;
use cosmic::iced::{self, Alignment, Border, Color, Length, Padding, Shadow, Vector};
use cosmic::surface::action::{LiveSettings, simple_layer_shell};
use cosmic::{Element, Task, widget};
use cosmic_comp_config::input::TouchpadOverride;
use futures::future::{AbortHandle, Aborted, abortable};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

pub static OSD_INDICATOR_ID: LazyLock<widget::Id> =
    LazyLock::new(|| widget::Id::new("osd-indicator".to_string()));

#[derive(Debug)]
pub enum Params {
    DisplayBrightness(f64),      // Rung ratio k/20.0 (hotkeys)
    DisplayBrightnessExact(f64), // Exact raw ratio raw/max (slider/arbitrary)
    DisplayToggle(DisplayMode),
    DisplayNumber(u32),
    KeyboardBrightness(f64),
    SinkVolume(u32, bool),
    SourceVolume(u32, bool),
    AirplaneMode(bool),
    TouchpadEnabled(TouchpadOverride),
}

impl Params {
    /// Lucide glyph for this indicator.
    ///
    /// Vendored under `res/icons/lucide` rather than pulled from the icon theme:
    /// the rest of the Humain shell draws Lucide, and `from_name` would resolve
    /// whatever the active icon theme happens to ship, which is a different
    /// drawing style entirely.
    fn icon_bytes(&self) -> &'static [u8] {
        use crate::icons::{
            KEYBOARD, LAPTOP, MIC, MIC_OFF, MONITOR, PLANE, SUN, TOUCHPAD, TOUCHPAD_OFF, VOLUME_1,
            VOLUME_2, VOLUME_X, WIFI,
        };

        match self {
            Self::DisplayBrightness(_) | Self::DisplayBrightnessExact(_) => SUN,
            Self::DisplayToggle(DisplayMode::All) => LAPTOP,
            Self::DisplayToggle(DisplayMode::External) => MONITOR,
            Self::DisplayNumber(_) => {
                unreachable!("DisplayNumber uses custom rendering and should not call icon_bytes()")
            }
            // Lucide has no keyboard-backlight glyph; the keyboard reads as the
            // thing being lit, and the bar beside it carries the level.
            Self::KeyboardBrightness(_) => KEYBOARD,
            // This OSD is icon-only, so the two states must differ by glyph:
            // a plane going into airplane mode, and radios back on coming out
            // of it. Lucide has no struck-through plane.
            Self::AirplaneMode(true) => PLANE,
            Self::AirplaneMode(false) => WIFI,
            Self::SinkVolume(volume, muted) => {
                if *volume == 0 || *muted {
                    VOLUME_X
                } else if *volume < 50 {
                    VOLUME_1
                } else {
                    VOLUME_2
                }
            }
            Self::SourceVolume(_, muted) => {
                if *muted {
                    MIC_OFF
                } else {
                    MIC
                }
            }
            Self::TouchpadEnabled(TouchpadOverride::None) => TOUCHPAD,
            Self::TouchpadEnabled(TouchpadOverride::ForceDisable) => TOUCHPAD_OFF,
        }
    }

    pub(crate) fn value(&self) -> Option<u32> {
        match self {
            Self::DisplayBrightness(value) => {
                let mut rung = (*value * 20.0).round() as u32;
                if rung > 20 {
                    rung = 20;
                }
                if rung == 0 && *value > 0.0 {
                    Some(1) // 1% at the floor
                } else {
                    Some(5 * rung)
                }
            }

            // SLIDER / EXACT: show precise percent from exact ratio, with 1% floor.
            Self::DisplayBrightnessExact(value) => {
                // round(100 * ratio)
                let mut p = (*value * 100.0).round() as i32;
                if p <= 0 && *value >= 0.0 {
                    p = 1;
                } // never show 0%
                if p > 100 {
                    p = 100;
                }
                Some(p as u32)
            }
            Self::KeyboardBrightness(value) => Some((*value * 100.) as u32),
            Self::SinkVolume(_, true) => Some(0),
            Self::SourceVolume(_, true) => Some(0),
            Self::SinkVolume(value, false) => Some(*value),
            Self::SourceVolume(value, false) => Some(*value),
            Self::AirplaneMode(_) => None,
            Self::TouchpadEnabled(_) => None,
            Self::DisplayToggle(_) => None,
            Self::DisplayNumber(_) => None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Msg {
    Ignore,
    /// Start going.
    Close(SurfaceId),
    /// Out of sight, so the surface can go too.
    Gone(SurfaceId),
}

/// How long the indicator takes to grow out of the corner, and to go back.
const ENTER: Duration = Duration::from_millis(260);
const EXIT: Duration = Duration::from_millis(200);
/// How long the bar takes to reach a new level.
const GLIDE: Duration = Duration::from_millis(160);
/// Clear space around the pill, for its shadow. The surface sits in the
/// screen's corner, so this is also the pill's distance from it.
const ROOM: f32 = 24.0;
const PILL_WIDTH: f32 = 300.0;
const PILL_HEIGHT: f32 = 48.0;
/// The pill's corner radius, which makes it a pill.
pub const PILL_RADIUS: u32 = 24;
/// The pill is fully grown by this much presence, and only then do its
/// contents start to fade in, so they never spill past it.
const GROWN_BY: f32 = 0.6;

/// A value easing toward a target, from wherever it was when the target moved.
#[derive(Debug, Clone, Copy)]
struct Tween {
    from: f32,
    to: f32,
    since: Instant,
    length: Duration,
}

impl Tween {
    fn settled(at: f32, now: Instant) -> Self {
        Self {
            from: at,
            to: at,
            since: now,
            length: Duration::ZERO,
        }
    }

    fn at(&self, now: Instant) -> f32 {
        let run = if self.length.is_zero() {
            1.0
        } else {
            (now.saturating_duration_since(self.since).as_secs_f32() / self.length.as_secs_f32())
                .clamp(0.0, 1.0)
        };
        // Coming in eases out: quick to answer the key, soft to land. Going
        // eases in and out, so the contents take a moment to fade before the
        // pill shrinks away.
        let eased = if self.to < self.from {
            if run < 0.5 {
                4.0 * run.powi(3)
            } else {
                1.0 - (-2.0 * run + 2.0).powi(3) / 2.0
            }
        } else {
            1.0 - (1.0 - run).powi(3)
        };
        self.from + (self.to - self.from) * eased
    }

    fn head(&mut self, to: f32, length: Duration, now: Instant) {
        if (self.to - to).abs() < f32::EPSILON {
            return;
        }
        self.from = self.at(now);
        self.to = to;
        self.since = now;
        self.length = length;
    }

    fn moving(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.since) < self.length
    }
}

#[derive(Debug)]
pub struct State {
    id: SurfaceId,
    params: Params,
    /// The pending close, or once closing, the pending removal.
    timer_abort: AbortHandle,
    pub margin: (i32, i32, i32, i32),
    amplification_sink: bool,
    amplification_source: bool,
    /// 0 out of sight, 1 in place.
    presence: Tween,
    /// The bar's fill, 0 to 1.
    level: Tween,
    /// The pill's size the compositor last blurred behind, in whole pixels.
    blurred: Option<(i32, i32)>,
}

/// Blur behind `size` of the pill, or nothing.
///
/// The compositor rounds each blurred rect with the surface's corner radius,
/// clamped to half its shorter side, so the blur keeps the pill's shape at
/// every size it grows through.
fn blur_behind(id: SurfaceId, size: Option<(i32, i32)>) -> Task<Msg> {
    use cosmic::iced::runtime::{Action, platform_specific, task};
    let rects = size.map(|(width, height)| {
        vec![iced::Rectangle {
            x: ROOM,
            y: ROOM,
            width: width as f32,
            height: height as f32,
        }]
    });
    task::effect(Action::PlatformSpecific(
        platform_specific::Action::Wayland(platform_specific::wayland::Action::BlurSurface(
            id, rects,
        )),
    ))
}

/// How far the pill has grown at `presence`, 0 to 1.
fn grown(presence: f32) -> f32 {
    (presence / GROWN_BY).clamp(0.0, 1.0)
}

/// How opaque its contents are at `presence`.
fn ink(presence: f32) -> f32 {
    ((presence - GROWN_BY) / (1.0 - GROWN_BY)).clamp(0.0, 1.0)
}

fn after(wait: Duration, msg: Msg) -> (Task<Msg>, AbortHandle) {
    let (future, timer_abort) = abortable(tokio::time::sleep(wait));
    let command = cosmic::task::future(async move {
        match future.await {
            Ok(()) => msg,
            Err(Aborted) => Msg::Ignore,
        }
    });
    (command, timer_abort)
}

fn close_timer(id: SurfaceId) -> (Task<Msg>, AbortHandle) {
    after(Duration::from_secs(3), Msg::Close(id))
}

/// Creates a 1-second timer for display identifiers
/// When the timer expires, it sends Msg::Close to remove the display identifier
fn display_identifier_timer(id: SurfaceId) -> (Task<Msg>, AbortHandle) {
    after(Duration::from_secs(1), Msg::Close(id))
}

impl State {
    pub fn new(
        id: SurfaceId,
        params: Params,
        margin: IcedMargin,
    ) -> (Self, Task<cosmic::Action<crate::components::app::Msg>>) {
        Self::new_with_output(id, params, IcedOutput::Active, margin)
    }

    pub fn new_with_output(
        id: SurfaceId,
        params: Params,
        output: IcedOutput,
        _margin: IcedMargin,
    ) -> (Self, Task<cosmic::Action<crate::components::app::Msg>>) {
        let mut cmds = vec![];

        let is_display_number = matches!(params, Params::DisplayNumber(_));
        // Display numbers mark each screen from its top edge. Everything else
        // sits in the top-left corner.
        let anchor = if is_display_number {
            Anchor::TOP
        } else {
            Anchor::TOP | Anchor::LEFT
        };

        // For display numbers, set exclusive_zone to -1 so they don't block input
        // in transparent areas. For other OSDs, use default behavior.
        let exclusive_zone = if is_display_number { -1 } else { 0 };
        let margin = if is_display_number {
            // Set top margin for display identifiers
            IcedMargin {
                top: 48,
                right: 0,
                bottom: 0,
                left: 0,
            }
        } else {
            // The pill keeps its own room from the corner.
            IcedMargin::default()
        };

        cmds.push(cosmic::surface::surface_task(simple_layer_shell(
            // The pill sets its own blur, the size it is as it grows: a
            // surface-wide one would frost the clear room around it.
            || LiveSettings {
                blur: Some(false),
                ..LiveSettings::default()
            },
            move || SctkLayerSurfaceSettings {
                id,
                keyboard_interactivity: KeyboardInteractivity::None,
                namespace: "osd".into(),
                layer: Layer::Overlay,
                size: None,
                anchor,
                output: output.clone(),
                exclusive_zone,
                margin,
                input_zone: Some(Vec::new()),
                ..Default::default()
            },
            None::<fn() -> Element<'static, cosmic::Action<Msg>>>,
        )));

        // Display numbers auto-close after 1 second, other OSDs after 3 seconds
        let (cmd, timer_abort) = if is_display_number {
            display_identifier_timer(id)
        } else {
            close_timer(id)
        };
        cmds.push(cmd.map(move |x| {
            if is_display_number {
                cosmic::action::app(crate::components::app::Msg::DisplayIdentifierSurface((
                    id, x,
                )))
            } else {
                cosmic::Action::App(crate::components::app::Msg::OsdIndicator(x))
            }
        }));

        let amplification_sink = config::amplification_sink();
        let amplification_source = config::amplification_source();

        let now = Instant::now();
        let mut state = Self {
            id,
            params,
            timer_abort,
            margin: (0, 0, 0, 0),
            amplification_sink,
            amplification_source,
            presence: Tween::settled(0.0, now),
            level: Tween::settled(0.0, now),
            blurred: None,
        };
        state.level = Tween::settled(state.fill(), now);
        if is_display_number {
            state.presence = Tween::settled(1.0, now);
        } else {
            state.presence.head(1.0, ENTER, now);
        }
        (state, Task::batch(cmds))
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Whether anything on it is still moving, so it needs frames.
    pub fn animating(&self, now: Instant) -> bool {
        self.presence.moving(now) || self.level.moving(now) || self.blurred != self.pill_pixels(now)
    }

    /// The pill's full width: a value has a bar beside its glyph.
    fn pill_width(&self) -> f32 {
        if self.params.value().is_some() {
            PILL_WIDTH
        } else {
            PILL_HEIGHT
        }
    }

    /// The pill's size `now`. It grows out of the corner over the first part of
    /// coming in, and shrinks back over the last part of going.
    fn pill_size(&self, now: Instant) -> (f32, f32) {
        let grown = grown(self.presence.at(now));
        (self.pill_width() * grown, PILL_HEIGHT * grown)
    }

    /// How opaque the glyph, bar and value are `now`: they fade in once the
    /// pill has grown, and go before it shrinks.
    fn ink(&self, now: Instant) -> f32 {
        ink(self.presence.at(now))
    }

    /// [`Self::pill_size`] in whole pixels, or `None` while too small to show.
    fn pill_pixels(&self, now: Instant) -> Option<(i32, i32)> {
        if matches!(self.params, Params::DisplayNumber(_)) {
            return None;
        }
        let (width, height) = self.pill_size(now);
        let size = (width.round() as i32, height.round() as i32);
        (size.0 > 0 && size.1 > 0).then_some(size)
    }

    /// Bring the blur behind the pill up to its size `now`.
    fn sync_blur(&mut self, now: Instant) -> Task<Msg> {
        let size = self.pill_pixels(now);
        if size == self.blurred {
            return Task::none();
        }
        self.blurred = size;
        blur_behind(self.id, size)
    }

    // Re-use OSD surface to show a different OSD
    // Resets close timer
    pub fn replace_params(&mut self, params: Params) -> Task<Msg> {
        let now = Instant::now();
        self.params = params;
        // A press while it is leaving brings it back from where it got to.
        self.presence.head(1.0, ENTER, now);
        self.level.head(self.fill(), GLIDE, now);
        // Reset timer
        self.timer_abort.abort();
        let (cmd, timer_abort) = close_timer(self.id);
        self.timer_abort = timer_abort;
        cmd
    }

    // Reset the timer for display identifiers
    // This is called when a new identify message is received to keep them visible
    pub fn reset_display_identifier_timer(&mut self) -> Task<Msg> {
        if !matches!(self.params, Params::DisplayNumber(_)) {
            return Task::none();
        }

        self.timer_abort.abort();
        let (cmd, timer_abort) = display_identifier_timer(self.id);
        self.timer_abort = timer_abort;
        cmd
    }

    fn max_value(&self) -> f32 {
        match self.params {
            Params::SinkVolume(_, _) if self.amplification_sink => 150.0,
            Params::SourceVolume(_, _) if self.amplification_source => 150.0,
            _ => 100.0,
        }
    }

    /// How full the bar should be for the current value.
    fn fill(&self) -> f32 {
        self.params.value().map_or(0.0, |value| {
            (value as f32 / self.max_value()).clamp(0.0, 1.0)
        })
    }

    pub fn view(&self) -> Element<'_, Msg> {
        // Display numbers use a completely different rendering
        if let Params::DisplayNumber(display_number) = self.params {
            return self.view_display_number(display_number);
        }

        let now = Instant::now();
        let (glass_width, glass_height) = self.pill_size(now);
        let grown = glass_height / PILL_HEIGHT;
        let shown = self.ink(now);
        let faded = move |color: Color, alpha: f32| Color {
            a: color.a * alpha * shown,
            ..color
        };
        let ink = Color::WHITE;
        let glyph = self.params.icon_bytes();

        let (content, width): (Element<'_, Msg>, f32) = if let Some(value) = self.params.value() {
            let level = self.level.at(now).clamp(0.0, 1.0);
            let accent: Color = cosmic::theme::active().cosmic().accent_color().into();
            let filled = (level * 1000.0).round() as u16;
            let fill = iced::widget::row![
                widget::container(widget::Space::new())
                    .width(Length::FillPortion(filled))
                    .height(Length::Fill)
                    .class(cosmic::theme::Container::custom(move |_| {
                        widget::container::Style {
                            background: Some(faded(accent, 1.0).into()),
                            border: Border::default().rounded(3.0),
                            ..Default::default()
                        }
                    })),
                widget::Space::new().width(Length::FillPortion(1000 - filled)),
            ]
            .height(Length::Fill);
            // Amplified, the bar runs to 150%: a tick marks where 100% is, or
            // 65% would read as under half.
            let max_value = self.max_value();
            let track: Element<'_, Msg> = if max_value > 100.0 {
                let at = (100.0 / max_value * 1000.0).round() as u16;
                iced::widget::stack![
                    fill,
                    iced::widget::row![
                        widget::Space::new().width(Length::FillPortion(at)),
                        widget::container(widget::Space::new())
                            .width(Length::Fixed(2.0))
                            .height(Length::Fill)
                            .class(cosmic::theme::Container::custom(move |_| {
                                widget::container::Style {
                                    background: Some(faded(ink, 0.6).into()),
                                    ..Default::default()
                                }
                            })),
                        widget::Space::new().width(Length::FillPortion(1000 - at)),
                    ]
                    .height(Length::Fill),
                ]
                .into()
            } else {
                fill.into()
            };
            let bar = widget::container(track)
                .width(Length::Fill)
                .height(Length::Fixed(6.0))
                .class(cosmic::theme::Container::custom(move |_| {
                    widget::container::Style {
                        background: Some(faded(ink, 0.16).into()),
                        border: Border::default().rounded(3.0),
                        ..Default::default()
                    }
                }));

            let row = iced::widget::row![
                lucide_icon(glyph, 20),
                bar,
                widget::text::body(format!("{value}%"))
                    .width(Length::Fixed(40.0))
                    .align_x(Alignment::End),
            ]
            .spacing(14)
            .align_y(Alignment::Center)
            .padding(Padding::from([0, 16]))
            .into();
            (row, PILL_WIDTH)
        } else {
            (lucide_icon(glyph, 24).into(), PILL_HEIGHT)
        };

        // Tinted glass over the compositor's blur, the size it has grown to.
        let glass = widget::container(widget::Space::new())
            .width(Length::Fixed(glass_width))
            .height(Length::Fixed(glass_height))
            .class(cosmic::theme::Container::custom(move |_| {
                widget::container::Style {
                    background: Some(Color::from_rgba(0.0, 0.0, 0.0, 0.45).into()),
                    border: Border {
                        radius: (glass_height / 2.0).into(),
                        width: 1.0,
                        color: Color::from_rgba(1.0, 1.0, 1.0, 0.12 * grown),
                    },
                    shadow: Shadow {
                        color: Color::from_rgba(0.0, 0.0, 0.0, 0.3 * grown),
                        offset: Vector::new(0.0, 6.0),
                        blur_radius: 18.0,
                    },
                    ..Default::default()
                }
            }));
        let contents = widget::container(content)
            .center_x(Length::Fixed(width))
            .center_y(Length::Fixed(PILL_HEIGHT))
            .class(cosmic::theme::Container::custom(move |_| {
                widget::container::Style {
                    text_color: Some(faded(ink, 0.92)),
                    icon_color: Some(faded(ink, 0.92)),
                    ..Default::default()
                }
            }));
        let pill = iced::widget::stack![
            widget::container(glass)
                .width(Length::Fixed(width))
                .height(Length::Fixed(PILL_HEIGHT)),
            contents,
        ];

        // The surface keeps its full size while the pill grows inside it, so it
        // is never resized mid-animation.
        widget::autosize::autosize(
            widget::container(pill).padding(ROOM),
            OSD_INDICATOR_ID.clone(),
        )
        .min_width(1.)
        .min_height(1.)
        .into()
    }

    fn view_display_number(&self, display_number: u32) -> Element<'_, Msg> {
        const CONTAINER_BASE_SIZE: u16 = 27;
        const TEXT_SIZE: u16 = 45;

        let theme = cosmic::theme::active();
        let cosmic_theme = theme.cosmic();

        let number_text = widget::text::title1(format!("{}", display_number))
            .size(TEXT_SIZE)
            .line_height(cosmic::iced::widget::text::LineHeight::Absolute(
                cosmic::iced::Pixels(TEXT_SIZE as f32),
            ))
            .width(Length::Shrink)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center);

        let content = widget::container(number_text).center(Length::Fill);

        let padding = cosmic_theme.space_l();
        let square_size = (CONTAINER_BASE_SIZE + (padding * 2)) as f32;

        let container = widget::container(content)
            .padding(padding)
            .center(Length::Fixed(square_size))
            .class(cosmic::theme::Container::custom(move |theme| {
                widget::container::Style {
                    text_color: Some(iced::Color::from(theme.cosmic().on_accent_color())),
                    background: Some(iced::Color::from(theme.cosmic().accent_color()).into()),
                    border: Border {
                        radius: theme.cosmic().radius_m().into(),
                        width: 0.0,
                        color: iced::Color::TRANSPARENT,
                    },
                    shadow: Default::default(),
                    icon_color: Some(iced::Color::from(theme.cosmic().on_accent_color())),
                    snap: true,
                }
            }));

        let autosize_id = iced::id::Id::new(format!("display-number-{}", display_number));
        widget::autosize::autosize(container, autosize_id)
            .min_width(1.)
            .min_height(1.)
            .into()
    }

    pub fn update(mut self, msg: Msg) -> (Option<Self>, Task<Msg>) {
        log::trace!("indicator msg: {:?}", msg);
        match msg {
            // Also each animation frame's tick, which carries the blur along.
            Msg::Ignore => {
                let blur = self.sync_blur(Instant::now());
                (Some(self), blur)
            }
            // Display numbers come and go with their screens' own surfaces.
            Msg::Close(id) if matches!(self.params, Params::DisplayNumber(_)) => {
                (None, destroy_layer_surface(id))
            }
            Msg::Close(id) => {
                let now = Instant::now();
                self.presence.head(0.0, EXIT, now);
                let (cmd, timer_abort) = after(EXIT, Msg::Gone(id));
                self.timer_abort = timer_abort;
                let blur = self.sync_blur(now);
                (Some(self), Task::batch([cmd, blur]))
            }
            Msg::Gone(id) => (None, destroy_layer_surface(id)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Tween;
    use std::time::{Duration, Instant};

    #[test]
    fn the_contents_never_show_on_a_pill_still_growing() {
        for step in 0..=100 {
            let presence = step as f32 / 100.0;
            if super::ink(presence) > 0.0 {
                assert!(
                    (super::grown(presence) - 1.0).abs() < f32::EPSILON,
                    "at {presence}"
                );
            }
        }
        assert!(super::grown(0.0).abs() < f32::EPSILON);
        assert!((super::ink(1.0) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn going_takes_its_time_before_the_pill_shrinks() {
        let start = Instant::now();
        let mut presence = Tween::settled(1.0, start);
        presence.head(0.0, super::EXIT, start);
        // A third of the way out the contents are still fading, not gone.
        let third = presence.at(start + super::EXIT / 3);
        assert!(super::ink(third) > 0.0, "{third}");
    }

    #[test]
    fn a_reversed_tween_turns_back_from_where_it_got_to() {
        let start = Instant::now();
        let mut presence = Tween::settled(0.0, start);
        presence.head(1.0, Duration::from_millis(200), start);
        let midway = start + Duration::from_millis(100);
        let there = presence.at(midway);
        assert!(
            there > 0.5 && there < 1.0,
            "eased out, so past halfway: {there}"
        );

        presence.head(0.0, Duration::from_millis(200), midway);
        assert!(
            (presence.at(midway) - there).abs() < 1e-6,
            "no jump on turning"
        );
        assert!(presence.moving(midway + Duration::from_millis(199)));
        assert!(!presence.moving(midway + Duration::from_millis(200)));
        assert!(presence.at(midway + Duration::from_millis(200)).abs() < 1e-6);
    }
}

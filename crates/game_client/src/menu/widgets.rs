//! Buttons, switches, sliders and text fields, and keeping them painted.

use super::*;

pub(super) fn font(size: f32) -> TextFont {
    TextFont {
        font_size: FontSize::Px(size),
        ..default()
    }
}

pub(super) fn text(value: impl Into<String>, size: f32, color: Color) -> impl Bundle {
    (Text::new(value), font(size), TextColor(color))
}

pub(super) fn background() -> BackgroundGradient {
    BackgroundGradient::from(LinearGradient::new(
        LinearGradient::TO_BOTTOM_RIGHT,
        vec![
            ColorStop::percent(Color::srgb(0.085, 0.105, 0.15), 0),
            ColorStop::percent(Color::srgb(0.03, 0.036, 0.05), 55),
            ColorStop::percent(Color::srgb(0.015, 0.018, 0.024), 100),
        ],
    ))
}

pub(super) fn button(
    p: &mut ChildSpawnerCommands,
    action: MenuButton,
    look: Look,
    label: impl Into<String>,
) {
    let (padding, size, color) = match look {
        Look::Nav => (UiRect::axes(px(16), px(10)), 22.0, TEXT),
        Look::Primary => (UiRect::axes(px(28), px(11)), 18.0, TEXT),
        _ => (UiRect::axes(px(14), px(8)), 15.0, TEXT),
    };
    p.spawn((
        Name::new(action.element_name()),
        action,
        look,
        Button,
        Node {
            padding,
            justify_content: if look == Look::Nav {
                JustifyContent::Start
            } else {
                JustifyContent::Center
            },
            align_items: AlignItems::Center,
            border_radius: BorderRadius::all(px(if look == Look::Nav { 8 } else { 6 })),
            ..default()
        },
        BackgroundColor(Color::NONE),
    ))
    .with_child(text(label, size, color));
}

/// A settings row: label on the left, control on the right.
pub(super) fn row(
    p: &mut ChildSpawnerCommands,
    label: &str,
    control: impl FnOnce(&mut ChildSpawnerCommands),
) {
    p.spawn(Node {
        min_height: px(40),
        align_items: AlignItems::Center,
        column_gap: px(16),
        ..default()
    })
    .with_children(|row| {
        row.spawn((
            Node {
                width: px(200),
                flex_shrink: 0.0,
                ..default()
            },
            children![text(label, 15.0, DIM)],
        ));
        row.spawn(Node {
            align_items: AlignItems::Center,
            column_gap: px(8),
            flex_wrap: FlexWrap::Wrap,
            row_gap: px(6),
            ..default()
        })
        .with_children(control);
    });
}

pub(super) fn switch(p: &mut ChildSpawnerCommands, toggle: Toggle) {
    let action = MenuButton::Toggle(toggle);
    p.spawn((
        Name::new(action.element_name()),
        action,
        Look::Custom,
        Button,
        Node {
            width: px(44),
            flex_shrink: 0.0,
            height: px(24),
            border_radius: BorderRadius::all(px(12)),
            ..default()
        },
        BackgroundColor(TRACK),
    ))
    .with_child((
        SwitchKnob(toggle),
        Node {
            position_type: PositionType::Absolute,
            left: px(3),
            top: px(3),
            width: px(18),
            height: px(18),
            border_radius: BorderRadius::all(px(9)),
            ..default()
        },
        BackgroundColor(TEXT),
    ));
}

pub(super) fn slider(p: &mut ChildSpawnerCommands, slider: Slider) {
    button(p, MenuButton::Step(slider, -1), Look::Plain, "-");
    p.spawn((
        Name::new(format!("slider:{}", slider.id())),
        SliderBar(slider),
        Look::Custom,
        Button,
        RelativeCursorPosition::default(),
        Node {
            width: px(200),
            height: px(24),
            align_items: AlignItems::Center,
            ..default()
        },
    ))
    .with_children(|bar| {
        bar.spawn((
            Node {
                width: percent(100),
                height: px(6),
                border_radius: BorderRadius::all(px(3)),
                ..default()
            },
            BackgroundColor(TRACK),
        ))
        .with_child((
            SliderFill(slider),
            Node {
                width: percent(50),
                height: percent(100),
                border_radius: BorderRadius::all(px(3)),
                ..default()
            },
            BackgroundColor(ACCENT),
        ));
        bar.spawn((
            SliderKnob(slider),
            Node {
                position_type: PositionType::Absolute,
                left: percent(50),
                top: px(4),
                width: px(16),
                height: px(16),
                margin: UiRect::left(px(-8)),
                border_radius: BorderRadius::all(px(8)),
                ..default()
            },
            BackgroundColor(TEXT),
        ));
    });
    button(p, MenuButton::Step(slider, 1), Look::Plain, "+");
    p.spawn((
        Value::Slider(slider),
        text("", 15.0, TEXT),
        Node {
            min_width: px(64),
            ..default()
        },
    ));
}

pub(super) fn text_field(p: &mut ChildSpawnerCommands, field: TextField, value: &str, width: f32) {
    let mut editable = EditableText::new(value);
    editable.max_characters = Some(match field {
        TextField::PlayerName => 24,
        TextField::Address => 64,
        TextField::Port => 5,
    });
    let mut text_entity = Entity::PLACEHOLDER;
    p.spawn((
        Node {
            width: px(width),
            flex_shrink: 0.0,
            padding: UiRect::axes(px(10), px(7)),
            border: UiRect::all(px(1)),
            border_radius: BorderRadius::all(px(6)),
            ..default()
        },
        BackgroundColor(FIELD),
        BorderColor::all(Color::NONE),
    ))
    .with_children(|b| {
        let mut entity = b.spawn((
            Name::new(format!("field:{}", format!("{field:?}").to_lowercase())),
            field,
            editable,
            font(16.0),
            TextColor(TEXT),
            Node {
                width: percent(100),
                ..default()
            },
        ));
        if field == TextField::Port {
            entity.insert(EditableTextFilter::new(|c| c.is_ascii_digit()));
        }
        text_entity = entity.id();
    })
    .insert(TextFieldBox(text_entity));
}

pub(super) fn heading(p: &mut ChildSpawnerCommands, title: &str, subtitle: &str) {
    p.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(4),
        margin: UiRect::bottom(px(18)),
        ..default()
    })
    .with_children(|h| {
        h.spawn(text(title, 30.0, TEXT));
        if !subtitle.is_empty() {
            h.spawn(text(subtitle, 15.0, DIM));
        }
    });
}

pub(super) fn section(p: &mut ChildSpawnerCommands, title: &str) {
    p.spawn((
        text(title.to_uppercase(), 12.0, DIM),
        Node {
            margin: UiRect::new(px(0), px(0), px(10), px(4)),
            ..default()
        },
    ));
}

pub(super) fn notice_box(p: &mut ChildSpawnerCommands, notice: &str) {
    p.spawn((
        Node {
            padding: UiRect::axes(px(14), px(10)),
            margin: UiRect::bottom(px(14)),
            max_width: px(640),
            border: UiRect::left(px(3)),
            border_radius: BorderRadius::all(px(6)),
            ..default()
        },
        BackgroundColor(ENEMY.with_alpha(0.14)),
        BorderColor::all(ENEMY),
        children![text(notice, 15.0, TEXT)],
    ));
}

pub(super) fn map_preview(
    p: &mut ChildSpawnerCommands,
    minimap: Option<&str>,
    size: f32,
    asset_server: &AssetServer,
) {
    let mut frame = p.spawn((
        Node {
            width: px(size),
            height: px(size),
            flex_shrink: 0.0,
            border_radius: BorderRadius::all(px(8)),
            overflow: Overflow::clip(),
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            ..default()
        },
        BackgroundColor(MAP_BACKGROUND),
    ));
    match minimap {
        Some(path) => {
            frame.with_child((
                ImageNode::new(asset_server.load(format!("imported://{path}"))),
                Node {
                    width: percent(100),
                    height: percent(100),
                    ..default()
                },
            ));
        }
        None => {
            frame.with_child(text("No map preview", 14.0, DIM));
        }
    }
}

fn is_selected(button: &MenuButton, menu: &Menu, settings: &Settings, browser: &ServerBrowser) -> bool {
    let last = &settings.last_match;
    match button {
        MenuButton::Server(index) => browser
            .entries
            .get(*index)
            .is_some_and(|e| e.port == last.port && e.address.eq_ignore_ascii_case(last.address.trim())),
        MenuButton::Page(page) => menu.page == *page,
        MenuButton::Level(name) => last.level == *name,
        MenuButton::Layout(mode, size) => last.mode == *mode && last.size == *size,
        MenuButton::Team(team) => last.team == *team && !last.spectate,
        MenuButton::Tab(tab) => menu.tab == *tab,
        MenuButton::Display(mode) => settings.window_mode == *mode,
        MenuButton::WindowSize(w, h) => settings.window_size == (*w, *h),
        MenuButton::Rebind(action) => menu.rebinding == Some(*action),
        _ => false,
    }
}

pub(super) fn paint_buttons(
    menu: Res<Menu>,
    settings: Res<Settings>,
    browser: Res<ServerBrowser>,
    mut buttons: Query<(&MenuButton, &Look, &Interaction, &mut BackgroundColor)>,
) {
    for (button, look, interaction, mut background) in &mut buttons {
        let hovered = *interaction != Interaction::None;
        let selected = is_selected(button, &menu, &settings, &browser);
        let color = match look {
            Look::Custom => continue,
            Look::Nav | Look::Item if selected => ACCENT.with_alpha(0.3),
            Look::Nav | Look::Item if hovered => HOVER.with_alpha(0.7),
            Look::Nav | Look::Item => Color::NONE,
            Look::Primary if hovered => ACCENT.lighter(0.08),
            Look::Primary => ACCENT,
            Look::Danger if hovered => ENEMY.with_alpha(0.55),
            _ if selected => ACCENT.with_alpha(0.45),
            _ if hovered => HOVER,
            _ => BUTTON,
        };
        background.set_if_neq(BackgroundColor(color));
    }
}

pub(super) fn paint_switches(
    settings: Res<Settings>,
    mut tracks: Query<(&MenuButton, &Interaction, &mut BackgroundColor), Without<SwitchKnob>>,
    mut knobs: Query<(&SwitchKnob, &mut Node)>,
) {
    for (button, interaction, mut background) in &mut tracks {
        let MenuButton::Toggle(toggle) = button else {
            continue;
        };
        let on = toggle.get(&settings);
        let mut color = if on { ACCENT } else { TRACK };
        if *interaction != Interaction::None {
            color = color.lighter(0.06);
        }
        background.set_if_neq(BackgroundColor(color));
    }
    for (knob, mut node) in &mut knobs {
        let left = px(if knob.0.get(&settings) { 23 } else { 3 });
        if node.left != left {
            node.left = left;
        }
    }
}

pub(super) fn paint_sliders(
    settings: Res<Settings>,
    mut fills: Query<(&SliderFill, &mut Node), Without<SliderKnob>>,
    mut knobs: Query<(&SliderKnob, &mut Node), Without<SliderFill>>,
) {
    for (fill, mut node) in &mut fills {
        let width = percent(fill.0.fraction(&settings) * 100.0);
        if node.width != width {
            node.width = width;
        }
    }
    for (knob, mut node) in &mut knobs {
        let left = percent(knob.0.fraction(&settings) * 100.0);
        if node.left != left {
            node.left = left;
        }
    }
}

pub(super) fn paint_text_fields(
    focus: Res<InputFocus>,
    mut boxes: Query<(&TextFieldBox, &mut BorderColor)>,
) {
    for (field, mut border) in &mut boxes {
        let color = if focus.get() == Some(field.0) {
            ACCENT
        } else {
            Color::NONE
        };
        border.set_if_neq(BorderColor::all(color));
    }
}

pub(super) fn update_values(
    menu: Res<Menu>,
    settings: Res<Settings>,
    mut texts: Query<(&Value, &mut Text)>,
) {
    for (value, mut text) in &mut texts {
        let line = match value {
            Value::Slider(slider) => slider.display(&settings),
            Value::Binding(action) if menu.rebinding == Some(*action) => "Press a key".into(),
            Value::Binding(action) => settings.binding(*action).label(),
        };
        if text.0 != line {
            text.0 = line;
        }
    }
}

//! One reusable text field for every input in the menus and the chat box (`crate::chat`).
//!
//! The caret, selection, word-wise editing (Ctrl+arrows, Ctrl+Backspace), Home/End, Ctrl+A
//! and OS clipboard (Ctrl+C/X/V) all come from Bevy's own [`EditableText`] widget and its
//! `bevy_ui_widgets` input plugin (already active via `DefaultPlugins`); this module adds:
//!
//! - a visible caret and selection highlight ([`TextCursorStyle`], which `EditableText` draws
//!   but doesn't add itself);
//! - a focus ring around the field's box ([`paint_focus_ring`]);
//! - Tab / Shift+Tab order among a page's fields (bevy's [`TabNavigationPlugin`], plus a
//!   [`TabGroup`] on each page's root in `menu::pages`);
//! - a password mask ([`PasswordMask`]; Bevy's widget doesn't have one yet);
//! - a character filter and single-line enforcement applied *after* every edit
//!   ([`sanitize_pasted_text`]) instead of Bevy's own [`EditableTextFilter`], which rejects an
//!   edit outright if even one character in it fails: fine for typing (always one character at
//!   a time) but it means a multi-character paste with a single bad character, or any newline
//!   in a paste, would otherwise be dropped whole instead of cleaned up.

use std::sync::Arc;

use bevy::{
    input_focus::{
        InputFocus,
        tab_navigation::{TabIndex, TabNavigationPlugin},
    },
    text::{EditableText, TextCursorStyle, TextEdit, TextEditChange},
    ui_widgets::ScrollIntoView,
};

use super::*;

pub struct TextInputPlugin;

impl Plugin for TextInputPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(TabNavigationPlugin)
            .add_observer(sanitize_pasted_text)
            .add_systems(
                Update,
                (paint_focus_ring, mask_passwords, scroll_focus_into_view).after(ScenarioSystems),
            );
    }
}

/// A field only digits pass (the port field).
pub fn digits(c: char) -> bool {
    c.is_ascii_digit()
}

/// What a [`text_input`] looks and behaves like beyond the plain default.
pub struct TextInputOptions {
    pub max_characters: Option<usize>,
    /// The real characters stay in the buffer (so editing, selection and clipboard all work
    /// normally on them) but are drawn invisibly; a [`PasswordMask`] sibling shows bullets
    /// instead, one per character.
    pub password: bool,
    /// Rejects (typing) or strips (a paste; see the module doc) any character it returns
    /// `false` for.
    pub filter: Option<Arc<dyn Fn(char) -> bool + Send + Sync>>,
    /// Tab order among the fields on the same page ([`TabGroup`]). Ties (the default, 0) are
    /// broken by spawn order, which is already top to bottom, so most fields can just leave
    /// this at 0.
    pub tab_index: i32,
}

impl Default for TextInputOptions {
    fn default() -> Self {
        Self { max_characters: None, password: false, filter: None, tab_index: 0 }
    }
}

/// The outer box of a text input, pointing at its [`EditableText`] child: the box gets the
/// focus ring ([`paint_focus_ring`]), since the child spans only the text itself.
#[derive(Component)]
pub struct TextInputBox(pub Entity);

/// Shows bullets over a password field's (invisible) text; `.0` is the real `EditableText`.
#[derive(Component)]
pub struct PasswordMask(pub Entity);

/// A field's character filter, applied after every edit rather than by Bevy's own
/// `EditableTextFilter` (see the module doc): `None` still enforces single-line, just without
/// filtering individual characters.
#[derive(Component, Clone, Default)]
struct PasteGuard(Option<Arc<dyn Fn(char) -> bool + Send + Sync>>);

/// Spawns a bordered text input: a real, focusable [`EditableText`] with a visible caret and
/// selection, a focus ring, Tab/Shift+Tab order and (optionally) a character filter or a
/// password mask. `marker` identifies the field to whatever reads it back (an enum naming
/// which setting it edits, for instance), `name` is its scenario element name
/// (`field:<name>`), and the returned `Entity` is the `EditableText` itself.
pub fn text_input(
    p: &mut ChildSpawnerCommands,
    marker: impl Bundle,
    name: impl std::fmt::Display,
    value: &str,
    width: f32,
    options: TextInputOptions,
) -> Entity {
    let mut editable = EditableText::new(value);
    editable.max_characters = options.max_characters;
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
        let entity = b.spawn((
            Name::new(format!("field:{name}")),
            marker,
            editable,
            font(16.0),
            TextColor(if options.password { Color::NONE } else { TEXT }),
            TextCursorStyle {
                color: ACCENT,
                selection_color: ACCENT.with_alpha(0.35),
                unfocused_selection_color: ACCENT.with_alpha(0.18),
                selected_text_color: None,
            },
            TabIndex(options.tab_index),
            // Not `EditableTextFilter`: Bevy's own filter rejects an edit outright if *any*
            // character in it fails (typing is always one character at a time, so that's fine
            // there, but it means a paste with even one disallowed character is dropped
            // whole instead of just losing that character - see `sanitize_pasted_text`, which
            // this drives instead and which strips rather than rejects).
            PasteGuard(options.filter.clone()),
            Node { width: percent(100), ..default() },
        ));
        text_entity = entity.id();
        if options.password {
            b.spawn((
                PasswordMask(text_entity),
                text("", 16.0, TEXT),
                Node {
                    position_type: PositionType::Absolute,
                    left: px(10),
                    top: px(7),
                    ..default()
                },
                Pickable::IGNORE,
            ));
        }
    })
    .insert(TextInputBox(text_entity));
    text_entity
}

/// The box's border glows while its field has keyboard focus.
fn paint_focus_ring(focus: Res<InputFocus>, mut boxes: Query<(&TextInputBox, &mut BorderColor)>) {
    for (field, mut border) in &mut boxes {
        let color = if focus.get() == Some(field.0) { ACCENT } else { Color::NONE };
        border.set_if_neq(BorderColor::all(color));
    }
}

/// Scrolls a field into view when it gains keyboard focus (Tab/Shift+Tab, or a click), so
/// tabbing to a field below the fold brings it on screen. Gamepad D-pad navigation among
/// buttons has its own focus (`Menu::gamepad_focus`, not `InputFocus`) and triggers
/// [`ScrollIntoView`] itself (`gamepad_menu_nav`).
fn scroll_focus_into_view(focus: Res<InputFocus>, mut commands: Commands) {
    if focus.is_changed()
        && let Some(entity) = focus.get()
    {
        commands.trigger(ScrollIntoView { entity });
    }
}

fn mask_passwords(mut masks: Query<(&PasswordMask, &mut Text)>, fields: Query<&EditableText>) {
    for (mask, mut text) in &mut masks {
        let len = fields.get(mask.0).map_or(0, |f| f.value().chars().count());
        let bullets = "\u{2022}".repeat(len);
        if text.0 != bullets {
            text.0 = bullets;
        }
    }
}

/// After any edit, keeps only the first line and the characters [`PasteGuard`] allows. Typing a
/// disallowed character technically inserts it for one edit before this reverts it (rather than
/// Bevy's own `EditableTextFilter`, which would refuse the keystroke instead), which is not
/// visible to the player since both happen within the same frame, before it's ever drawn.
fn sanitize_pasted_text(change: On<TextEditChange>, mut fields: Query<(&mut EditableText, &PasteGuard)>) {
    let Ok((mut editable, guard)) = fields.get_mut(change.event_target()) else {
        return;
    };
    let current = editable.value().to_string();
    let first_line = current.split(['\n', '\r']).next().unwrap_or("");
    let cleaned: String = match &guard.0 {
        Some(allowed) => first_line.chars().filter(|c| allowed(*c)).collect(),
        None => first_line.to_string(),
    };
    if cleaned != current {
        editable.editor_mut().set_text(&cleaned);
        editable.queue_edit(TextEdit::TextEnd(false));
    }
}

//! The look shared by every screen that draws its own text over the 3D scene (the HUD, the
//! menus, the deploy and commander screens, chat, the radio wheel, ...): font sizing and the
//! drop shadow that keeps small text readable against whatever is behind it. Each of them used
//! to define its own copy of `font()` and `shadow()`; kept as one so they can't quietly drift
//! apart (they hadn't yet, but nothing stopped it).

use bevy::prelude::*;

/// A `TextFont` at `size` pixels (Bevy's default font).
pub fn font(size: f32) -> TextFont {
    TextFont {
        font_size: FontSize::Px(size),
        ..default()
    }
}

/// A tight drop shadow; the default 4 px offset reads as a second copy of small text.
pub fn shadow() -> TextShadow {
    TextShadow {
        offset: Vec2::splat(1.0),
        color: Color::srgba(0.0, 0.0, 0.0, 0.8),
    }
}

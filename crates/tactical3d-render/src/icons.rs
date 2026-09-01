//! Color UI icons (DESIGN.md §15): a small vendored set of Twemoji PNGs
//! (`assets/icons/`, CC-BY 4.0 — see ATTRIBUTION.md there) plus a few
//! project-original PIL glyphs (`tools/make_icon_candidates.py`),
//! all compiled into the exe and uploaded as egui textures at startup.
//!
//! Why not an emoji FONT: egui rasterizes glyphs as alpha masks, so color
//! emoji fonts (Segoe UI Emoji, Noto Color Emoji) cannot work; and the
//! `egui-twemoji` crate was rejected (pins `twemoji-assets` = embedding all
//! ~3700 emojis into the exe for the handful we need, plus an
//! egui_extras/resvg loader stack). Why not inline images: egui text cannot
//! embed images, so icons are composed next to text (`icon_button`,
//! `label_with_icon`) — the standard workaround.
//!
//! Every helper degrades to text-only when the [`IconSet`] resource is empty
//! (decode failure), so a missing texture never breaks the UI.

use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};
use std::collections::HashMap;

/// Semantic icon identifiers (file names match `assets/icons/<file>.png`).
///
/// Sources: most are vendored Twemoji (see ATTRIBUTION.md); the ones marked
/// "generated" are project-original PIL glyphs from
/// `tools/make_icon_candidates.py` — they replaced Twemojis whose semantics
/// did not match the button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IconId {
    Attack,     // ⚔️ crossed swords — assault / attack
    Defense,    // 🛡️ shield — defense / hold / cover
    Fire,       // 🔥 fire — barrage / artillery
    Target,     // 🎯 direct hit — direct fire
    Explosion,  // 💥 collision — annihilated
    Surrender,  // 🏳️ white flag — surrender
    Skull,      // ☠️ skull & crossbones — HQ destroyed
    Hourglass,  // ⏱️ stopwatch — hour complete
    Scroll,     // 📜 scroll — battle log
    Gear,       // ⚙️ gear — settings
    Trophy,     // 🏆 trophy — victory
    Dove,       // 🕊️ dove — defeat / withdrawal
    Flag,       // 🚩 flag — objective / VP city
    Eye,        // 👁️ eye — recon / sight
    Cross,      // ❌ cross mark — cancel / close / failure
    Warning,    // ⚠️ warning — cautions
    Check,      // ✅ check — confirm / ok
    Reset,      // 🔄 arrows — reset
    Door,       // 🚪 door — exit
    Clock,      // 🕐 clock — hour / turn time
    Save,       // 💾 floppy — save
    Back,       // 🔙 back arrow — recall to the OOB
    Horse,      // 🐎 horse — cavalry / movement
    Oob,        // generated: org tree — order of battle
    Map,        // generated: folded map — minimap
    Deploy,     // generated: pawn onto a hex zone — auto deploy
    Viewfinder, // generated: viewfinder corners — reset camera view
    Advance,    // generated: double chevron — division order: advance
    BackArrow,  // generated: plain left arrow — menu back
    Listen,     // generated: field radio — live listen
    Sync,       // generated: two chasing arrows — sync to HOI4
}

const ICONS: &[(IconId, &str, &[u8])] = &[
    (
        IconId::Attack,
        "attack",
        include_bytes!("../../../assets/icons/attack.png"),
    ),
    (
        IconId::Defense,
        "defense",
        include_bytes!("../../../assets/icons/defense.png"),
    ),
    (
        IconId::Fire,
        "fire",
        include_bytes!("../../../assets/icons/fire.png"),
    ),
    (
        IconId::Target,
        "target",
        include_bytes!("../../../assets/icons/target.png"),
    ),
    (
        IconId::Explosion,
        "explosion",
        include_bytes!("../../../assets/icons/explosion.png"),
    ),
    (
        IconId::Surrender,
        "surrender",
        include_bytes!("../../../assets/icons/surrender.png"),
    ),
    (
        IconId::Skull,
        "skull",
        include_bytes!("../../../assets/icons/skull.png"),
    ),
    (
        IconId::Hourglass,
        "hourglass",
        include_bytes!("../../../assets/icons/hourglass.png"),
    ),
    (
        IconId::Scroll,
        "scroll",
        include_bytes!("../../../assets/icons/scroll.png"),
    ),
    (
        IconId::Gear,
        "gear",
        include_bytes!("../../../assets/icons/gear.png"),
    ),
    (
        IconId::Trophy,
        "trophy",
        include_bytes!("../../../assets/icons/trophy.png"),
    ),
    (
        IconId::Dove,
        "dove",
        include_bytes!("../../../assets/icons/dove.png"),
    ),
    (
        IconId::Flag,
        "flag",
        include_bytes!("../../../assets/icons/flag.png"),
    ),
    (
        IconId::Eye,
        "eye",
        include_bytes!("../../../assets/icons/eye.png"),
    ),
    (
        IconId::Cross,
        "cross",
        include_bytes!("../../../assets/icons/cross.png"),
    ),
    (
        IconId::Warning,
        "warning",
        include_bytes!("../../../assets/icons/warning.png"),
    ),
    (
        IconId::Check,
        "check",
        include_bytes!("../../../assets/icons/check.png"),
    ),
    (
        IconId::Reset,
        "reset",
        include_bytes!("../../../assets/icons/reset.png"),
    ),
    (
        IconId::Door,
        "door",
        include_bytes!("../../../assets/icons/door.png"),
    ),
    (
        IconId::Clock,
        "clock",
        include_bytes!("../../../assets/icons/clock.png"),
    ),
    (
        IconId::Save,
        "save",
        include_bytes!("../../../assets/icons/save.png"),
    ),
    (
        IconId::Back,
        "back",
        include_bytes!("../../../assets/icons/back.png"),
    ),
    (
        IconId::Horse,
        "horse",
        include_bytes!("../../../assets/icons/horse.png"),
    ),
    (
        IconId::Oob,
        "oob",
        include_bytes!("../../../assets/icons/oob.png"),
    ),
    (
        IconId::Map,
        "map",
        include_bytes!("../../../assets/icons/map.png"),
    ),
    (
        IconId::Deploy,
        "deploy",
        include_bytes!("../../../assets/icons/deploy.png"),
    ),
    (
        IconId::Viewfinder,
        "viewfinder",
        include_bytes!("../../../assets/icons/viewfinder.png"),
    ),
    (
        IconId::Advance,
        "advance",
        include_bytes!("../../../assets/icons/advance.png"),
    ),
    (
        IconId::BackArrow,
        "back_arrow",
        include_bytes!("../../../assets/icons/back_arrow.png"),
    ),
    (
        IconId::Listen,
        "listen",
        include_bytes!("../../../assets/icons/listen.png"),
    ),
    (
        IconId::Sync,
        "sync",
        include_bytes!("../../../assets/icons/sync.png"),
    ),
];

/// Uploaded egui textures, keyed by [`IconId`]. Empty until the startup
/// system runs; helpers fall back to text-only in that case.
///
/// The full `TextureHandle` must be stored, not just the `TextureId`: the
/// handle is ref-counted and egui frees the texture when the last handle
/// drops — keeping only the id silently killed every icon one frame after
/// upload (buttons fell back to text-only).
#[derive(Resource, Default)]
pub struct IconSet {
    textures: HashMap<IconId, egui::TextureHandle>,
}

impl IconSet {
    /// Decode the embedded PNGs and upload them as egui textures.
    pub fn load(&mut self, ctx: &egui::Context) {
        for (id, name, bytes) in ICONS {
            let img = match image::load_from_memory(bytes) {
                Ok(img) => img.to_rgba8(),
                Err(e) => {
                    eprintln!("[icons] decode {name}.png failed: {e}");
                    continue;
                }
            };
            let (w, h) = img.dimensions();
            let color = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &img);
            let tex = ctx.load_texture(format!("icon-{name}"), color, egui::TextureOptions::LINEAR);
            self.textures.insert(*id, tex);
        }
    }

    pub fn tex(&self, id: IconId) -> Option<egui::TextureId> {
        self.textures.get(&id).map(egui::TextureHandle::id)
    }

    fn image(&self, id: IconId, size: f32) -> Option<egui::Image<'static>> {
        self.tex(id)
            .map(|t| egui::Image::new(egui::load::SizedTexture::new(t, egui::vec2(size, size))))
    }

    /// A button with an icon left of the text; plain text button when the
    /// texture is missing. Caller decides sizing/enabling (`ui.add(...)`,
    /// `ui.add_sized(...)`, `ui.add_enabled(...)`).
    pub fn button<'a>(
        &self,
        id: Option<IconId>,
        text: impl Into<egui::WidgetText>,
        size: f32,
    ) -> egui::Button<'a> {
        match id.and_then(|i| self.image(i, size)) {
            Some(img) => egui::Button::image_and_text(img, text.into()),
            None => egui::Button::new(text.into()),
        }
    }

    /// `ui.horizontal` icon + label; plain label when the texture is missing.
    pub fn label_with_icon(
        &self,
        ui: &mut egui::Ui,
        id: IconId,
        text: impl Into<egui::WidgetText>,
        size: f32,
    ) -> egui::Response {
        if let Some(img) = self.image(id, size) {
            ui.horizontal(|ui| {
                ui.add(img);
                ui.label(text.into())
            })
            .inner
        } else {
            ui.label(text.into())
        }
    }

    /// Just the icon image (e.g. log-line leading icons).
    pub fn icon(&self, ui: &mut egui::Ui, id: IconId, size: f32) {
        if let Some(img) = self.image(id, size) {
            ui.add(img);
        }
    }
}

/// Bevy system: upload the textures exactly once, as soon as the egui
/// context exists (same first-frame `try_ctx_mut` discipline as fonts.rs).
pub fn init_icons_once(
    mut contexts: EguiContexts,
    mut icons: ResMut<IconSet>,
    mut done: Local<bool>,
) {
    if *done {
        return;
    }
    let Some(ctx) = contexts.try_ctx_mut() else {
        return;
    };
    icons.load(ctx);
    *done = true;
}

#[cfg(test)]
mod tests {
    /// The vendored Twemoji PNGs are palette-encoded (8-bit and 4-bit
    /// colormap + tRNS). The `image` crate must decode them to RGBA with a
    /// real alpha channel — if it ever misreads the palette, every icon
    /// silently renders invisible.
    #[test]
    fn twemoji_pngs_decode_with_alpha() {
        for (_id, name, bytes) in super::ICONS {
            let img = image::load_from_memory(bytes)
                .unwrap_or_else(|e| panic!("{name}.png decode: {e}"))
                .to_rgba8();
            let opaque = img.pixels().filter(|p| p.0[3] > 0).count();
            assert!(
                opaque > 500,
                "{name}.png: only {opaque} non-transparent pixels after decode"
            );
        }
    }
}

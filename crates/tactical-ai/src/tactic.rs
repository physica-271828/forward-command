//! HOI4 combat tactics — the strategic driver of the AI (DESIGN §7.2).
//!
//! Tokens mirror `common/combat_tactics.txt` (extracted into
//! `data/combat_tactics.json`, §5.4). The AI receives the enemy tactic via the
//! `tac_enemy_tactic` log message (§3.1) and maps it to a strategic objective.
//!
//! The card library holds 16 cards: every one of the 55 vanilla HOI4
//! tokens single-maps onto one of them (the vanilla fallbacks
//! `basic_attack` / `basic_defend` and unknown tokens fold to `Default`).
//! Cards are NOT side-locked: any card may be assigned to either side
//! (headless `atk_tactic=`/`def_tactic=`, scripts, debug form). The
//! side-attribute in the doc comments is the HOI4 bloodline only —
//! vanilla attacker-only rolls (`cc_withdraw` / the sf_storm assault
//! family) never fold onto the defender card of the same family, because
//! the live enemy AI plays the attacked side's real roll; and the
//! planner lifts a Default attacker onto the plain-advance posture so
//! the generic attack roll still moves.

use std::fmt;

/// The 16 tactics the tactical AI can play (§7.2 mapping table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CombatTactic {
    Blitz,
    ElasticDefense,
    OverwhelmingFire,
    InfiltrationAssault,
    MassCharge,
    GuerrillaTactics,
    TacticalWithdrawal,
    Encirclement,
    /// Fallback for `basic_attack` / `basic_defend` / unknown tokens (and
    /// the mod's placeholder "default"). Dual posture: a defender holds
    /// and engages the nearest enemy; an attacker is lifted onto the
    /// plain advance by the planner.
    Default,
    /// Defender counter-offensive — hold the line, but strike hard
    /// at an isolated/beaten target when the window opens, then hold the
    /// ground taken (unlike elastic defense, which strikes and falls back).
    Counterattack,
    /// Defender ambush — never move, lurk in cover, strike only at
    /// an enemy that steps adjacent.
    Ambush,
    /// Defender river line — never retreat, hit the enemy
    /// mid-ford.
    RiverDefense,
    /// Defender urban fight — garrison the city, never leave it,
    /// fight enemies that enter.
    UrbanDefense,
    /// Defender mobile delay — keep the enemy in a 2–3 hex
    /// contact band with constant fire and stepwise resistance, never
    /// breaking contact (vs tactical_withdrawal's full rearward shift).
    Delay,
    /// Attacker standard assault — artillery preparation on the
    /// weakest hex, infantry line advances in step, assault on contact.
    Assault,
    /// Attacker river crossing — artillery on the ford, infantry
    /// forces the crossing (accepting the ×2 river damage), then holds the
    /// far bank.
    RiverAssault,
}

impl CombatTactic {
    /// Parse a HOI4 tactic token (§7.2). The full vanilla token
    /// set maps onto the 16 cards (phase-variants fold into their parent's
    /// behavior; masterful variants fold into their base). Unknown tokens
    /// fall back to `Default`.
    pub fn from_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "blitz" | "masterful_blitz" | "breakthrough" => CombatTactic::Blitz,
            "elastic_defense" => CombatTactic::ElasticDefense,
            "overwhelming_fire" => CombatTactic::OverwhelmingFire,
            "infiltration_assault" => CombatTactic::InfiltrationAssault,
            // Mass-charge family: banzai / human wave / infantry charge /
            // shock / close-combat storm all behave as one relentless line.
            "mass_charge"
            | "banzai_charge"
            | "grand_banzai_charge"
            | "human_wave_tactics"
            | "infantry_charge"
            | "shock"
            | "cc_storm" => CombatTactic::MassCharge,
            "guerrilla_tactics" => CombatTactic::GuerrillaTactics,
            // Withdrawal family: the base card plus its defender-phase rolls.
            // The attacker's close-combat disengagement (cc_withdraw) instead
            // folds into Assault — an AI attacker must never hold a
            // rearward-posture card.
            "tactical_withdrawal" | "tw_defend" | "tw_evade" => {
                CombatTactic::TacticalWithdrawal
            }
            "encirclement" => CombatTactic::Encirclement,
            "counterattack" | "backhand_blow" => CombatTactic::Counterattack,
            "ambush" | "cc_local_strong_point" => CombatTactic::Ambush,
            "delay" | "masterful_delay" => CombatTactic::Delay,
            // Standard-assault family: the base card, its close-combat /
            // withdrawal-phase attacker rolls (incl. cc_withdraw, the
            // attacker's disengagement), the four vanilla "generic attack"
            // variants (incl. barrage, the attacker's only artillery card —
            // it belongs to the assault flow's fire-preparation step, not
            // the defender-side overwhelming_fire), and the attacker's
            // street-fighting assault rolls.
            "assault" | "planned_attack" | "relentless_assault" | "unexpected_thrust"
            | "barrage" | "cc_attack" | "cc_defend" | "cc_withdraw"
            | "tw_attack" | "tw_chase" | "tw_intercept"
            | "sf_storm" | "sf_barrage" | "sf_armor_supported_assault" | "sf_mouse_holing" => {
                CombatTactic::Assault
            }
            // Bridge family: attacker forces the crossing.
            "seize_bridge"
            | "attacker_sb_hold"
            | "attacker_sb_skillful_defence"
            | "attacker_hb_attack"
            | "attacker_hb_rush"
            | "attacker_hb_storm" => CombatTactic::RiverAssault,
            // Bridge family: defender holds the river line.
            "hold_bridge"
            | "defender_sb_assault"
            | "defender_sb_reckless_assault"
            | "defender_sb_retake_bridge"
            | "defender_hb_hold"
            | "defender_hb_skillful_defence" => CombatTactic::RiverDefense,
            // Urban family, defender rolls only. The attacker's street
            // assault rolls (sf_storm / sf_barrage /
            // sf_armor_supported_assault / sf_mouse_holing, all
            // is_attacker=yes in vanilla) fold into Assault — as
            // UrbanDefense an AI attacker would garrison outside the city
            // and never storm it.
            "urban_defense" | "sf_defense" | "sf_fortify" | "sf_ambush" => {
                CombatTactic::UrbanDefense
            }
            // Vanilla fallbacks and anything unknown fold to Default — the
            // planner lifts a Default attacker onto the plain-advance
            // posture (TacticalAi::new), so the generic attack roll still
            // prosecutes the advance.
            "basic_attack" | "basic_defend" => CombatTactic::Default,
            _ => CombatTactic::Default,
        }
    }

    /// HOI4 script token (§5.4 combat_tactics.json keys) — the card's own
    /// canonical token; the from_str mapping accepts all 55 vanilla tokens.
    pub fn token(self) -> &'static str {
        match self {
            CombatTactic::Blitz => "blitz",
            CombatTactic::ElasticDefense => "elastic_defense",
            CombatTactic::OverwhelmingFire => "overwhelming_fire",
            CombatTactic::InfiltrationAssault => "infiltration_assault",
            CombatTactic::MassCharge => "mass_charge",
            CombatTactic::GuerrillaTactics => "guerrilla_tactics",
            CombatTactic::TacticalWithdrawal => "tactical_withdrawal",
            CombatTactic::Encirclement => "encirclement",
            CombatTactic::Default => "default",
            CombatTactic::Counterattack => "counterattack",
            CombatTactic::Ambush => "ambush",
            CombatTactic::RiverDefense => "hold_bridge",
            CombatTactic::UrbanDefense => "urban_defense",
            CombatTactic::Delay => "delay",
            CombatTactic::Assault => "assault",
            CombatTactic::RiverAssault => "seize_bridge",
        }
    }

    /// Human-readable name for the Tactic Card UI panel (§9.1).
    pub fn name(self) -> &'static str {
        match self {
            CombatTactic::Blitz => "Blitz",
            CombatTactic::ElasticDefense => "Elastic Defense",
            CombatTactic::OverwhelmingFire => "Overwhelming Fire",
            CombatTactic::InfiltrationAssault => "Infiltration Assault",
            CombatTactic::MassCharge => "Mass Charge",
            CombatTactic::GuerrillaTactics => "Guerrilla Tactics",
            CombatTactic::TacticalWithdrawal => "Tactical Withdrawal",
            CombatTactic::Encirclement => "Encirclement",
            CombatTactic::Default => "Default",
            CombatTactic::Counterattack => "Counterattack",
            CombatTactic::Ambush => "Ambush",
            CombatTactic::RiverDefense => "River Defense",
            CombatTactic::UrbanDefense => "Urban Defense",
            CombatTactic::Delay => "Delay",
            CombatTactic::Assault => "Assault",
            CombatTactic::RiverAssault => "River Assault",
        }
    }

    /// One-line behavior summary (§7.2 Behavior column), shown on the
    /// Tactic Card (§9.1).
    pub fn description(self) -> &'static str {
        match self {
            CombatTactic::Blitz => {
                "Deep penetration: armor rushes a narrow front, bypassing strongpoints; motorized follows to widen the breach."
            }
            CombatTactic::ElasticDefense => {
                "Delay and preserve: falls back when attacked, counter-attacks only isolated enemy units."
            }
            CombatTactic::OverwhelmingFire => {
                "Attrition warfare: all artillery concentrates on the weakest hex while the infantry line holds."
            }
            CombatTactic::InfiltrationAssault => {
                "Exploit gaps: recon probes the flanks; infantry concentrates on weakly-held hexes, avoiding strong positions."
            }
            CombatTactic::MassCharge => {
                "Full frontal assault: all infantry advances together, at most one hex per turn. High casualty tolerance."
            }
            CombatTactic::GuerrillaTactics => {
                "Hit and run: strike, then disengage — never ends the turn adjacent to the enemy."
            }
            CombatTactic::TacticalWithdrawal => {
                "Systematic retreat: falls back one hex per turn toward the rear behind a rearguard."
            }
            CombatTactic::Encirclement => {
                "Pincer movement: armor swings around both flanks while infantry pins the center."
            }
            CombatTactic::Default => {
                "Hold and engage the nearest enemy; damaged units rotate to the rear."
            }
            CombatTactic::Counterattack => {
                "Counter-offensive: holds the line, but strikes hard at an isolated or beaten enemy and keeps the ground."
            }
            CombatTactic::Ambush => {
                "Ambush: never moves, lurks in cover, strikes only an enemy that steps adjacent."
            }
            CombatTactic::RiverDefense => {
                "River line: holds the riverbank at all costs — no retreat, half-forded enemies hit double."
            }
            CombatTactic::UrbanDefense => {
                "Street fighting: garrisons the city, never leaves it, fights every enemy that enters."
            }
            CombatTactic::Delay => {
                "Mobile delay: keeps the enemy in a 2-3 hex contact band with constant fire and stepwise resistance."
            }
            CombatTactic::Assault => {
                "Standard assault: artillery prepares the weakest hex, the infantry line advances in step, assaults on contact."
            }
            CombatTactic::RiverAssault => {
                "River crossing: artillery pounds the ford, infantry forces the crossing and holds the far bank."
            }
        }
    }

    /// Counter relationship from `combat_tactics.json` (§5.4 `counters` /
    /// `countered_by` fields), shown as a hint on the Tactic Card (§9.1).
    pub fn counter_hint(self) -> &'static str {
        match self {
            // blitz.countered_by = ["elastic_defense"]
            CombatTactic::Blitz => "Countered by Elastic Defense",
            // elastic_defense.counters = ["blitz", "masterful_blitz"]
            CombatTactic::ElasticDefense => "Counters Blitz",
            // overwhelming_fire.counters = ["banzai_charge", ...] (mass charges)
            CombatTactic::OverwhelmingFire => "Counters Mass Charge",
            CombatTactic::InfiltrationAssault => "No direct counter",
            // banzai/grand_banzai (mass charges) countered_by overwhelming_fire
            CombatTactic::MassCharge => "Countered by Overwhelming Fire",
            CombatTactic::GuerrillaTactics => "No direct counter",
            // tactical_withdrawal.counters = ["encirclement"]
            CombatTactic::TacticalWithdrawal => "Counters Encirclement",
            // encirclement.countered_by = ["tactical_withdrawal"]
            CombatTactic::Encirclement => "Countered by Tactical Withdrawal",
            CombatTactic::Default => "No counter",
            // counterattack.counters = ["basic_attack", "assault"]
            CombatTactic::Counterattack => "Counters Assault",
            // ambush.counters = ["shock"]; ambush.countered_by = ["breakthrough"]
            CombatTactic::Ambush => "Counters Shock",
            // hold_bridge family holds the fords against seize_bridge (defender_hb_skillful_defence.counters = ["attacker_hb_storm"])
            CombatTactic::RiverDefense => "Counters River Assault",
            CombatTactic::UrbanDefense => "No direct counter",
            // delay.countered_by = ["shock"]
            CombatTactic::Delay => "Countered by Shock",
            // assault.countered_by = ["counterattack"]
            CombatTactic::Assault => "Countered by Counterattack",
            // seize_bridge family forced by the river line (attacker_sb_skillful_defence counters the retake)
            CombatTactic::RiverAssault => "Countered by River Defense",
        }
    }
}

impl fmt::Display for CombatTactic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl std::str::FromStr for CombatTactic {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(CombatTactic::from_str(s))
    }
}

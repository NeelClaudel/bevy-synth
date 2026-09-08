//! Play the synth from the computer keyboard, tracker-style.
//!
//! Two rows of keys give two octaves, in the layout every tracker and DAW uses:
//! `Z S X D C V G B H N J M` is the lower octave with the black keys on the row
//! above, and `Q 2 W 3 E R 5 T 6 Y 7 U` is the octave above that. `[` and `]`
//! transpose.
//!
//! Add [`keyboard_input`] to `Update` and it plays. It is a convenience for
//! prototyping and for players without MIDI hardware — a real MIDI keyboard
//! bypasses the ECS entirely and has lower latency (see [`crate::MidiMode`]).

use bevy_ecs::prelude::*;
use bevy_input::keyboard::KeyCode;
use bevy_input::ButtonInput;

use crate::Synth;

/// Which octave the computer keyboard plays in.
#[derive(Resource, Debug, Clone, Copy)]
pub struct KeyboardOctave(pub i32);

impl Default for KeyboardOctave {
    fn default() -> Self {
        // Octave 4 puts `Z` on middle C, matching every tracker's default.
        Self(4)
    }
}

/// Key-to-semitone map, as an offset from the C of the current octave.
const KEY_MAP: &[(KeyCode, i32)] = &[
    // Lower octave: the home row is the white keys.
    (KeyCode::KeyZ, 0),  // C
    (KeyCode::KeyS, 1),  // C#
    (KeyCode::KeyX, 2),  // D
    (KeyCode::KeyD, 3),  // D#
    (KeyCode::KeyC, 4),  // E
    (KeyCode::KeyV, 5),  // F
    (KeyCode::KeyG, 6),  // F#
    (KeyCode::KeyB, 7),  // G
    (KeyCode::KeyH, 8),  // G#
    (KeyCode::KeyN, 9),  // A
    (KeyCode::KeyJ, 10), // A#
    (KeyCode::KeyM, 11), // B
    (KeyCode::Comma, 12),
    // Upper octave.
    (KeyCode::KeyQ, 12),
    (KeyCode::Digit2, 13),
    (KeyCode::KeyW, 14),
    (KeyCode::Digit3, 15),
    (KeyCode::KeyE, 16),
    (KeyCode::KeyR, 17),
    (KeyCode::Digit5, 18),
    (KeyCode::KeyT, 19),
    (KeyCode::Digit6, 20),
    (KeyCode::KeyY, 21),
    (KeyCode::Digit7, 22),
    (KeyCode::KeyU, 23),
    (KeyCode::KeyI, 24),
];

/// Translates key presses into notes.
///
/// Add to `Update`, and add [`KeyboardOctave`] as a resource (or use
/// `init_resource`).
pub fn keyboard_input(
    keys: Res<ButtonInput<KeyCode>>,
    synth: Res<Synth>,
    mut octave: ResMut<KeyboardOctave>,
) {
    // Transpose first, so a key held across an octave change still releases the
    // note it started — the release below is computed from the same offset the
    // press used only if the octave has not moved, which is why the note-offs
    // are sent before applying a new octave.
    let shift_down = keys.just_pressed(KeyCode::BracketLeft);
    let shift_up = keys.just_pressed(KeyCode::BracketRight);

    if shift_down || shift_up {
        // Release everything before transposing: otherwise the note-off would
        // be computed in the new octave and the old note would hang forever.
        synth.all_notes_off();
        if shift_down {
            octave.0 = (octave.0 - 1).max(0);
        } else {
            octave.0 = (octave.0 + 1).min(8);
        }
        return;
    }

    let base = (octave.0 + 1) * 12;

    for &(key, offset) in KEY_MAP {
        let note = base + offset;
        if !(0..=127).contains(&note) {
            continue;
        }
        let note = note as u8;

        if keys.just_pressed(key) {
            synth.note_on(note, 0.85);
        }
        if keys.just_released(key) {
            synth.note_off(note);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_key_maps_to_two_notes() {
        for (i, (key, _)) in KEY_MAP.iter().enumerate() {
            for (other_key, _) in KEY_MAP.iter().skip(i + 1) {
                assert_ne!(key, other_key, "{key:?} is mapped twice");
            }
        }
    }

    #[test]
    fn the_map_covers_two_full_octaves() {
        let mut offsets: Vec<i32> = KEY_MAP.iter().map(|(_, o)| *o).collect();
        offsets.sort_unstable();
        offsets.dedup();
        assert_eq!(offsets.first(), Some(&0));
        assert_eq!(offsets.last(), Some(&24));
        // Every semitone in between must be reachable.
        for semitone in 0..=24 {
            assert!(offsets.contains(&semitone), "no key plays semitone {semitone}");
        }
    }
}

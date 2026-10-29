//! The Paneru command vocabulary.
//!
//! Every way of telling the window manager to do something funnels through
//! [`Command`]: the TOML `[bindings]` table, the `send-cmd` socket protocol, an
//! embedded Lua `init.lua`, and the loadable Lua client module. This crate owns
//! the types and their argv encoding ([`parse_command`] / [`Command::to_argv`]).

use serde::{Deserialize, Serialize};

pub use crate::types::argv::{ParseError, parse_command};

/// Represents a cardinal or directional choice for window manipulation.
///
/// Deserializes from either a direction name (`"east"`) or a 1-based position
/// (`3`, meaning the third column), which is what both the argv encoding and the
/// Lua API accept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    North,
    South,
    West,
    East,
    First,
    Last,
    Nth(usize),
}

impl Direction {
    #[must_use]
    pub fn reverse(&self) -> Self {
        match self {
            Direction::North => Direction::South,
            Direction::South => Direction::North,
            Direction::West => Direction::East,
            Direction::East => Direction::West,
            Direction::First => Direction::Last,
            Direction::Last => Direction::First,
            Direction::Nth(index) => Direction::Nth(*index),
        }
    }

    /// Parses a direction name. Positions are not accepted; see
    /// [`Direction::parse_positional`].
    ///
    /// # Errors
    ///
    /// Returns [`ParseError`] if `name` is not a recognized direction name.
    pub fn parse(name: &str) -> Result<Self, ParseError> {
        Ok(match name {
            "north" => Direction::North,
            "south" => Direction::South,
            "west" => Direction::West,
            "east" => Direction::East,
            "first" => Direction::First,
            "last" => Direction::Last,
            other => return Err(ParseError::new(format!("unhandled direction '{other}'"))),
        })
    }

    /// Parses `north`, `south`, `west`, or `east`.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError`] if `name` is not one of the four compass directions.
    pub fn parse_compass(name: &str) -> Result<Self, ParseError> {
        match Direction::parse(name)? {
            Direction::First | Direction::Last | Direction::Nth(_) => Err(ParseError::new(
                format!("'{name}' is not north, south, west, or east"),
            )),
            direction => Ok(direction),
        }
    }

    /// Parses a direction name or a 1-based position (`"3"` → `Nth(2)`).
    ///
    /// # Errors
    ///
    /// Returns [`ParseError`] if `input` is neither a recognized direction name
    /// nor a positive integer.
    pub fn parse_positional(input: &str) -> Result<Self, ParseError> {
        match input.parse::<usize>() {
            Ok(0) => Err(ParseError::new("window numbers start at 1")),
            Ok(number) => Ok(Direction::Nth(number - 1)),
            Err(_) => Direction::parse(input),
        }
    }

    /// The argv token this direction encodes to.
    #[must_use]
    pub fn token(&self) -> String {
        match self {
            Direction::North => "north".into(),
            Direction::South => "south".into(),
            Direction::West => "west".into(),
            Direction::East => "east".into(),
            Direction::First => "first".into(),
            Direction::Last => "last".into(),
            Direction::Nth(index) => (index + 1).to_string(),
        }
    }
}

/// The plain, externally tagged spelling of [`Direction`], used on the wire.
///
/// A binary format cannot decode the flexible `"east"`-or-`3` form below —
/// `untagged` works by asking the format what the next value *is*, which only a
/// self-describing one can answer. This mirror carries the same variants with a
/// derived impl, so the wire gets a discriminant and a payload.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum DirectionRepr {
    North,
    South,
    West,
    East,
    First,
    Last,
    Nth(usize),
}

impl From<DirectionRepr> for Direction {
    fn from(repr: DirectionRepr) -> Self {
        match repr {
            DirectionRepr::North => Self::North,
            DirectionRepr::South => Self::South,
            DirectionRepr::West => Self::West,
            DirectionRepr::East => Self::East,
            DirectionRepr::First => Self::First,
            DirectionRepr::Last => Self::Last,
            DirectionRepr::Nth(index) => Self::Nth(index),
        }
    }
}

impl<'de> Deserialize<'de> for Direction {
    /// From a human-readable format, accepts `"east"` or `3`, so a Lua caller
    /// can write either `{ direction = "east" }` or `{ number = 3 }` and get the
    /// same enum. From a binary one — the wire — reads the derived spelling,
    /// because the flexible form is undecodable there and unnecessary: nothing
    /// hand-writes a request.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// The two spellings a human-readable format accepts.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Name(String),
            Position(i64),
        }

        if !deserializer.is_human_readable() {
            return DirectionRepr::deserialize(deserializer).map(Direction::from);
        }

        match Repr::deserialize(deserializer)? {
            Repr::Name(name) => Direction::parse(&name).map_err(serde::de::Error::custom),
            Repr::Position(number) => usize::try_from(number)
                .ok()
                .filter(|number| *number > 0)
                .map(|number| Direction::Nth(number - 1))
                .ok_or_else(|| serde::de::Error::custom("window numbers start at 1")),
        }
    }
}

/// The horizontal neighbour a stacking operation targets: the column to the
/// left (`West`) or to the right (`East`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    West,
    East,
}

impl Side {
    /// # Errors
    ///
    /// Returns [`ParseError`] if `input` is neither `west` nor `east`.
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        Ok(match input {
            "west" => Side::West,
            "east" => Side::East,
            other => return Err(ParseError::new(format!("unhandled side '{other}'"))),
        })
    }

    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Side::West => "west",
            Side::East => "east",
        }
    }

    /// The compass direction of this side.
    #[must_use]
    pub fn direction(self) -> Direction {
        match self {
            Side::West => Direction::West,
            Side::East => Direction::East,
        }
    }

    /// The end of a strip on this side: its first or last column.
    #[must_use]
    pub fn end(self) -> Direction {
        match self {
            Side::West => Direction::First,
            Side::East => Direction::Last,
        }
    }
}

/// How far an edge command may go once its strip's end column has focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reach {
    /// On to the display on that side.
    Display,
    /// No farther: a held edge key runs this, so only a fresh press crosses.
    Strip,
}

/// Direction used when cycling preset resize widths and heights.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResizeDirection {
    Grow,
    Shrink,
}

impl ResizeDirection {
    /// # Errors
    ///
    /// Returns [`ParseError`] if `input` is not a recognized resize direction.
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        Ok(match input {
            "grow" => ResizeDirection::Grow,
            "shrink" => ResizeDirection::Shrink,
            other => {
                return Err(ParseError::new(format!(
                    "unhandled resize direction '{other}'"
                )));
            }
        })
    }

    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            ResizeDirection::Grow => "grow",
            ResizeDirection::Shrink => "shrink",
        }
    }
}

/// Controls whether focus follows the window after a move operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveFocus {
    Follow,
    Stay,
}

impl MoveFocus {
    /// `follow = true` is the default everywhere a caller can choose.
    #[must_use]
    pub fn follows(follow: bool) -> Self {
        if follow { Self::Follow } else { Self::Stay }
    }
}

/// A 1-based virtual workspace number as written by users, stored 0-based.
///
/// # Errors
///
/// Returns [`ParseError`] if `input` is not a positive integer.
pub fn parse_virtual_workspace_number(input: &str) -> Result<u32, ParseError> {
    let number = input
        .parse::<u32>()
        .map_err(|_| ParseError::new(format!("unhandled virtual workspace '{input}'")))?;
    if number == 0 {
        return Err(ParseError::new("virtual workspace numbers start at 1"));
    }
    Ok(number - 1)
}

/// Defines the various operations that can be performed on windows.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    /// Focuses on a window in the specified `Direction`.
    Focus(Direction),
    /// Swaps the current window with another in the specified `Direction`.
    Swap(Direction),
    /// Centers the currently focused window on the display.
    Center,
    /// Resizes the focused window in the given direction.
    Resize(ResizeDirection),
    /// Cycles the focused window's height through the preset stack heights.
    /// Only applies to windows inside a stack.
    ResizeVertical(ResizeDirection),
    /// Grows or shrinks the focused column's width by `window_resize_step` of
    /// the display width. It stops at the limits instead of wrapping.
    ResizeStep(ResizeDirection),
    /// Grows or shrinks the focused window's height by `window_resize_step` of
    /// the display height, taking the difference from its neighbour. Only
    /// applies to windows inside a stack, and stops at the limits.
    ResizeVerticalStep(ResizeDirection),
    /// Resizes the focused window to an exact display-width ratio.
    SetWidth(f64),
    /// Maximizes the focused window, or restores it when it is maximized, as
    /// a double-click on its title bar does. `full_width` is its older name.
    #[serde(alias = "full_width")]
    Maximize,
    /// Moves the focused window's whole column to the next available display.
    ToNextDisplay(MoveFocus),
    /// Focuses the display that lies north, south, west, or east of the active
    /// display in the physical arrangement.
    FocusDisplay(Direction),
    /// Moves the focused window's whole column to the display that lies
    /// north, south, west, or east of the active display, and follows it there.
    MoveToDisplay(Direction),
    /// Focuses the first (`West`) or last (`East`) column of the active strip.
    /// When that column already has focus, or the strip has no column, focuses
    /// the display on that side instead, as `FocusDisplay` does, if `Reach`
    /// allows it.
    FocusEdge(Side, Reach),
    /// Moves the focused window's whole column to the first (`West`) or last
    /// (`East`) position of its strip. When it is already there, moves the
    /// column on to the display on that side instead, as `MoveToDisplay` does,
    /// if `Reach` allows it.
    MoveEdge(Side, Reach),
    /// Distributes heights equally among windows in the focused stack.
    Equalize,
    /// Makes all columns in the active strip the same width as the focused window.
    Balance,
    /// Toggles the managed state of the focused window.
    Manage,
    /// Moves the focused window out of its column to the bottom of the
    /// neighbouring column on the given side.
    Stack(Side),
    /// Moves the focused window out of its stack into a new column on the
    /// given side of the stack.
    Unstack(Side),
    /// Toggles the focused window's stack between a normal split display
    /// (all windows visible, sharing height) and a tabbed display (one
    /// window visible at a time, sharing the full column). Cycle tabs with
    /// the existing `Focus` North/South directions.
    ToggleTabbedDisplay,
    /// Resizes and repositions the focused window to fit within the visible viewport
    /// (including edge padding).
    Snap,
    /// Cyclically selects the virtual strip for the current workspace.
    Virtual(Direction),
    /// North/South only: focuses a stack neighbor above/below the focused
    /// window if one exists (the same within-column traversal as `Focus`),
    /// otherwise cyclically selects the virtual strip like `Virtual` would.
    /// A distinct command from `Virtual` so a plain virtual-workspace-switch
    /// binding keeps meaning exactly that, unconditionally. A no-op on any
    /// other direction.
    FocusOrVirtual(Direction),
    /// Selects a virtual strip by its zero-based index for the current workspace.
    VirtualNumber(u32),
    /// Creates a new empty virtual strip after the highest existing one for
    /// the current workspace, and switches to it.
    VirtualAdd,
    /// Moves the focused window to the virtual strip.
    VirtualMove(Direction, MoveFocus),
    /// Moves the focused window to a virtual strip by its zero-based index.
    VirtualMoveNumber(u32, MoveFocus),
    /// Focuses the workspace's last-focused floating window.
    FocusUnmanaged,
    /// Focuses the workspace's last-focused managed (tiled) window.
    FocusManaged,
    /// Raises all visible floating windows on the active display and focuses
    /// the last-floating window (idempotent — repeat presses behave the same).
    RaiseFloating,
    /// Alt-tab between the floating and tiled tiers of the active workspace.
    /// Flips `FloatingLayer`, raises the other windows in the new top tier,
    /// and focuses the tier's last-focused window.
    ToggleFloatingLayer,
    /// Copies a `[windows]` configuration rule for the focused window to the
    /// clipboard.
    CopyRule,
}

/// Defines operations that can be performed on the mouse.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseMove {
    /// Moves the mouse pointer to the next available display.
    ToNextDisplay,
}

/// Represents a command that can be issued to the window manager.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    /// A command targeting a window with a specific `Operation`.
    Window(Operation),
    /// A command targeting the mouse with a specific `MouseOperation`.
    Mouse(MouseMove),
    /// A command to quit the window manager application.
    Quit,
    /// A command to restart the window manager service.
    Restart,
    PrintState,
    /// Invokes a Lua keybind handler by its registry id (see the daemon's
    /// `crate::lua`). Never produced by parsing; the runtime issues it directly.
    Lua(u32),
    /// Layout operations a Lua handler produced by transforming a `WindowSet`.
    /// Window-addressed, unlike every other command here, and applied
    /// best-effort: see `ecs::layout_ops`. Never produced by parsing.
    Layout(Vec<crate::types::windowset::LayoutOp>),
    /// Shows or hides the menu bar item and remembers the choice.
    MenuBar(bool),
}

impl Command {
    /// The command a key auto-repeat of this one runs: an edge command that
    /// stops at its strip's end. Every other command repeats unchanged.
    #[must_use]
    pub fn repeated(self) -> Self {
        match self {
            Command::Window(Operation::FocusEdge(side, _)) => {
                Command::Window(Operation::FocusEdge(side, Reach::Strip))
            }
            Command::Window(Operation::MoveEdge(side, _)) => {
                Command::Window(Operation::MoveEdge(side, Reach::Strip))
            }
            other => other,
        }
    }
}

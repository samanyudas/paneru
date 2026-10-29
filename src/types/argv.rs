//! The argv encoding of a [`Command`]: `["window", "focus", "east"]`.
//!
//! This is the wire format of the `send-cmd` socket protocol and the shape the
//! TOML `[bindings]` keys are split into, so parsing and formatting live
//! together here and are checked against each other by round-trip tests.

use crate::types::commands::{
    Command, Direction, MouseMove, MoveFocus, Operation, Reach, ResizeDirection, Side,
    parse_virtual_workspace_number,
};

/// Why an argv vector is not a command. Consumers wrap this in their own error
/// type; the message is already user-facing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError(String);

impl ParseError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    fn invalid(argv: &[&str]) -> Self {
        Self(format!("invalid command '{argv:?}'"))
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

type Result<T> = std::result::Result<T, ParseError>;

/// Parses a command argument vector into a [`Command`] (e.g. `["window",
/// "focus", "east"]`).
///
/// # Errors
///
/// Returns [`ParseError`] if `argv` is not a recognized command encoding.
pub fn parse_command(argv: &[&str]) -> Result<Command> {
    let command = *argv.first().unwrap_or(&"");
    Ok(match command {
        "printstate" => Command::PrintState,
        "window" => Command::Window(parse_operation(&argv[1..])?),
        "mouse" => Command::Mouse(parse_mouse_move(&argv[1..])?),
        "quit" => Command::Quit,
        "restart" => Command::Restart,
        "showmenu" if argv.len() == 1 => Command::MenuBar(true),
        "hidemenu" if argv.len() == 1 => Command::MenuBar(false),
        _ => return Err(ParseError::new(format!("unhandled command '{argv:?}'"))),
    })
}

/// Parses a window operation (e.g. `["focus", "east"]`).
fn parse_operation(argv: &[&str]) -> Result<Operation> {
    let command = *argv.first().unwrap_or(&"");
    let err = || ParseError::invalid(argv);
    let argument = || argv.get(1).ok_or_else(err).copied();

    Ok(match command {
        // The bindings key splits on `_`, so `window_focus_unmanaged` arrives
        // here as ["focus", "unmanaged"] and the suffix tells us the variant.
        "focus" => match argument()? {
            "unmanaged" => Operation::FocusUnmanaged,
            "managed" => Operation::FocusManaged,
            direction => Operation::Focus(Direction::parse_positional(direction)?),
        },
        "raise" => match argument()? {
            "floating" => Operation::RaiseFloating,
            _ => return Err(err()),
        },
        "togglefloatlayer" => Operation::ToggleFloatingLayer,
        "swap" => Operation::Swap(Direction::parse(argument()?)?),
        "center" => Operation::Center,
        "resize" => Operation::Resize(
            argv.get(1)
                .map_or(Ok(ResizeDirection::Grow), |arg| ResizeDirection::parse(arg))?,
        ),
        "grow" => Operation::Resize(ResizeDirection::Grow),
        "shrink" => Operation::Resize(ResizeDirection::Shrink),
        // `window_step_grow` / `_shrink`, growing when the direction is absent.
        "step" => Operation::ResizeStep(resize_direction_or_grow(argv.get(1).copied())?),
        // `window_vertical_resize` / `_grow` / `_shrink`, plus the explicit
        // `window_vertical_resize_shrink` mirroring `window_resize_shrink`, and
        // `window_vertical_step_grow` / `_shrink`.
        "vertical" => match argv.get(1).copied() {
            None | Some("resize") => {
                Operation::ResizeVertical(resize_direction_or_grow(argv.get(2).copied())?)
            }
            Some("step") => {
                Operation::ResizeVerticalStep(resize_direction_or_grow(argv.get(2).copied())?)
            }
            Some(arg) => Operation::ResizeVertical(ResizeDirection::parse(arg)?),
        },
        // `fullwidth` is the older name, which existing configs still bind.
        "maximize" | "fullwidth" => Operation::Maximize,
        "manage" => Operation::Manage,
        "equalize" => Operation::Equalize,
        "balance" => Operation::Balance,
        "stack" => Operation::Stack(side_or(argv.get(1).copied(), Side::West)?),
        "unstack" => Operation::Unstack(side_or(argv.get(1).copied(), Side::East)?),
        "tabbeddisplay" => Operation::ToggleTabbedDisplay,
        "nextdisplay" => Operation::ToNextDisplay(MoveFocus::Follow),
        "nextdisplaysend" => Operation::ToNextDisplay(MoveFocus::Stay),
        "focusdisplay" => Operation::FocusDisplay(Direction::parse_compass(argument()?)?),
        "movedisplay" => Operation::MoveToDisplay(Direction::parse_compass(argument()?)?),
        "focusedge" => Operation::FocusEdge(Side::parse(argument()?)?, Reach::Display),
        "moveedge" => Operation::MoveEdge(Side::parse(argument()?)?, Reach::Display),
        "snap" => Operation::Snap,
        "copyrule" => Operation::CopyRule,
        // The `virtual*` verbs take either a direction or a workspace number,
        // and `num` variants that only take a number.
        "virtual" => virtual_target(argument()?, Operation::Virtual, Operation::VirtualNumber)?,
        // No numeric form: "focus a stack neighbor, else switch" only makes
        // sense paired with a direction.
        "virtualfocus" => Operation::FocusOrVirtual(Direction::parse(argument()?)?),
        "virtualnum" => Operation::VirtualNumber(parse_virtual_workspace_number(argument()?)?),
        "virtualadd" => Operation::VirtualAdd,
        "virtualmove" => virtual_target(
            argument()?,
            |direction| Operation::VirtualMove(direction, MoveFocus::Follow),
            |index| Operation::VirtualMoveNumber(index, MoveFocus::Follow),
        )?,
        "virtualmovenum" => Operation::VirtualMoveNumber(
            parse_virtual_workspace_number(argument()?)?,
            MoveFocus::Follow,
        ),
        "virtualsend" => virtual_target(
            argument()?,
            |direction| Operation::VirtualMove(direction, MoveFocus::Stay),
            |index| Operation::VirtualMoveNumber(index, MoveFocus::Stay),
        )?,
        "virtualsendnum" => Operation::VirtualMoveNumber(
            parse_virtual_workspace_number(argument()?)?,
            MoveFocus::Stay,
        ),
        _ => return Err(err()),
    })
}

/// The resize verbs grow when the direction is omitted.
fn resize_direction_or_grow(argument: Option<&str>) -> Result<ResizeDirection> {
    argument.map_or(Ok(ResizeDirection::Grow), ResizeDirection::parse)
}

/// `stack` and `unstack` keep their original meaning when the side is omitted.
fn side_or(argument: Option<&str>, default: Side) -> Result<Side> {
    argument.map_or(Ok(default), Side::parse)
}

/// Resolves a `virtual*` argument that may be a direction or a 1-based number.
fn virtual_target(
    target: &str,
    directional: impl Fn(Direction) -> Operation,
    numbered: impl Fn(u32) -> Operation,
) -> Result<Operation> {
    if target.parse::<u32>().is_ok() {
        Ok(numbered(parse_virtual_workspace_number(target)?))
    } else {
        Ok(directional(Direction::parse(target)?))
    }
}

/// Parses a mouse command (e.g. `["nextdisplay"]`).
fn parse_mouse_move(argv: &[&str]) -> Result<MouseMove> {
    match *argv.first().unwrap_or(&"") {
        "nextdisplay" => Ok(MouseMove::ToNextDisplay),
        _ => Err(ParseError::new(format!("invalid mouse command '{argv:?}'"))),
    }
}

impl Command {
    /// The argv encoding of this command, as understood by [`parse_command`].
    ///
    /// [`Command::Lua`] and [`Command::Layout`] have no encoding — they are
    /// only ever issued in-process — and yield `None`.
    #[must_use]
    pub fn to_argv(&self) -> Option<Vec<String>> {
        let argv = match self {
            Command::Window(operation) => {
                let mut argv = vec!["window".to_string()];
                argv.extend(operation.to_argv());
                argv
            }
            Command::Mouse(MouseMove::ToNextDisplay) => {
                vec!["mouse".to_string(), "nextdisplay".to_string()]
            }
            Command::Quit => vec!["quit".to_string()],
            Command::Restart => vec!["restart".to_string()],
            Command::PrintState => vec!["printstate".to_string()],
            Command::MenuBar(enabled) => {
                vec![if *enabled { "showmenu" } else { "hidemenu" }.to_string()]
            }
            Command::Lua(_) | Command::Layout(_) => return None,
        };
        Some(argv)
    }
}

impl Operation {
    /// The argv tail following `window`, e.g. `["focus", "east"]`.
    fn to_argv(&self) -> Vec<String> {
        let owned = |args: &[&str]| args.iter().map(|arg| (*arg).to_string()).collect();
        match self {
            Operation::Focus(direction) => vec!["focus".to_string(), direction.token()],
            Operation::Swap(direction) => vec!["swap".to_string(), direction.token()],
            Operation::Center => owned(&["center"]),
            Operation::Resize(direction) => owned(&["resize", direction.token()]),
            Operation::ResizeVertical(direction) => {
                owned(&["vertical", "resize", direction.token()])
            }
            Operation::ResizeStep(direction) => owned(&["step", direction.token()]),
            Operation::ResizeVerticalStep(direction) => {
                owned(&["vertical", "step", direction.token()])
            }
            // `SetWidth` comes from window rules, not from a command line, and
            // has no argv verb of its own.
            Operation::SetWidth(_) | Operation::Maximize => owned(&["maximize"]),
            Operation::ToNextDisplay(MoveFocus::Follow) => owned(&["nextdisplay"]),
            Operation::ToNextDisplay(MoveFocus::Stay) => owned(&["nextdisplaysend"]),
            Operation::FocusDisplay(direction) => {
                vec!["focusdisplay".to_string(), direction.token()]
            }
            Operation::MoveToDisplay(direction) => {
                vec!["movedisplay".to_string(), direction.token()]
            }
            Operation::FocusEdge(side, _) => owned(&["focusedge", side.token()]),
            Operation::MoveEdge(side, _) => owned(&["moveedge", side.token()]),
            Operation::Equalize => owned(&["equalize"]),
            Operation::Balance => owned(&["balance"]),
            Operation::Manage => owned(&["manage"]),
            Operation::Stack(side) => owned(&["stack", side.token()]),
            Operation::Unstack(side) => owned(&["unstack", side.token()]),
            Operation::ToggleTabbedDisplay => owned(&["tabbeddisplay"]),
            Operation::Snap => owned(&["snap"]),
            Operation::Virtual(direction) => vec!["virtual".to_string(), direction.token()],
            Operation::FocusOrVirtual(direction) => {
                vec!["virtualfocus".to_string(), direction.token()]
            }
            Operation::VirtualNumber(index) => {
                vec!["virtualnum".to_string(), (index + 1).to_string()]
            }
            Operation::VirtualAdd => owned(&["virtualadd"]),
            Operation::VirtualMove(direction, MoveFocus::Follow) => {
                vec!["virtualmove".to_string(), direction.token()]
            }
            Operation::VirtualMove(direction, MoveFocus::Stay) => {
                vec!["virtualsend".to_string(), direction.token()]
            }
            Operation::VirtualMoveNumber(index, MoveFocus::Follow) => {
                vec!["virtualmovenum".to_string(), (index + 1).to_string()]
            }
            Operation::VirtualMoveNumber(index, MoveFocus::Stay) => {
                vec!["virtualsendnum".to_string(), (index + 1).to_string()]
            }
            Operation::FocusUnmanaged => owned(&["focus", "unmanaged"]),
            Operation::FocusManaged => owned(&["focus", "managed"]),
            Operation::RaiseFloating => owned(&["raise", "floating"]),
            Operation::ToggleFloatingLayer => owned(&["togglefloatlayer"]),
            Operation::CopyRule => owned(&["copyrule"]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(command: &Command) -> Command {
        let argv = command.to_argv().expect("command should encode to argv");
        let borrowed: Vec<&str> = argv.iter().map(String::as_str).collect();
        parse_command(&borrowed).unwrap_or_else(|err| panic!("re-parsing {argv:?}: {err}"))
    }

    #[test]
    fn binding_keys_name_the_step_and_display_commands() {
        let parse = |key: &str| parse_command(&key.split('_').collect::<Vec<_>>());
        let window = |operation| Ok(Command::Window(operation));
        assert_eq!(
            parse("window_step_shrink"),
            window(Operation::ResizeStep(ResizeDirection::Shrink))
        );
        assert_eq!(
            parse("window_step"),
            window(Operation::ResizeStep(ResizeDirection::Grow))
        );
        assert_eq!(
            parse("window_vertical_step_grow"),
            window(Operation::ResizeVerticalStep(ResizeDirection::Grow))
        );
        assert_eq!(
            parse("window_focusdisplay_east"),
            window(Operation::FocusDisplay(Direction::East))
        );
        assert_eq!(
            parse("window_movedisplay_south"),
            window(Operation::MoveToDisplay(Direction::South))
        );
        assert!(parse("window_focusdisplay_first").is_err());
        assert!(parse("window_movedisplay_2").is_err());
    }

    #[test]
    fn binding_keys_name_the_edge_commands() {
        let parse = |key: &str| parse_command(&key.split('_').collect::<Vec<_>>());
        let window = |operation| Ok(Command::Window(operation));
        assert_eq!(
            parse("window_focusedge_west"),
            window(Operation::FocusEdge(Side::West, Reach::Display))
        );
        assert_eq!(
            parse("window_focusedge_east"),
            window(Operation::FocusEdge(Side::East, Reach::Display))
        );
        assert_eq!(
            parse("window_moveedge_west"),
            window(Operation::MoveEdge(Side::West, Reach::Display))
        );
        assert_eq!(
            parse("window_moveedge_east"),
            window(Operation::MoveEdge(Side::East, Reach::Display))
        );
        for key in [
            "window_focusedge",
            "window_focusedge_north",
            "window_moveedge_first",
            "window_moveedge_2",
        ] {
            assert!(parse(key).is_err(), "{key} should not parse");
        }
    }

    #[test]
    fn every_operation_round_trips_through_argv() {
        let operations = [
            Operation::Focus(Direction::East),
            Operation::Focus(Direction::Nth(2)),
            Operation::Swap(Direction::West),
            Operation::Center,
            Operation::Resize(ResizeDirection::Shrink),
            Operation::ResizeVertical(ResizeDirection::Grow),
            Operation::ResizeVertical(ResizeDirection::Shrink),
            Operation::ResizeStep(ResizeDirection::Grow),
            Operation::ResizeStep(ResizeDirection::Shrink),
            Operation::ResizeVerticalStep(ResizeDirection::Grow),
            Operation::ResizeVerticalStep(ResizeDirection::Shrink),
            Operation::Maximize,
            Operation::ToNextDisplay(MoveFocus::Follow),
            Operation::ToNextDisplay(MoveFocus::Stay),
            Operation::FocusDisplay(Direction::North),
            Operation::FocusDisplay(Direction::West),
            Operation::MoveToDisplay(Direction::South),
            Operation::MoveToDisplay(Direction::East),
            Operation::FocusEdge(Side::West, Reach::Display),
            Operation::FocusEdge(Side::East, Reach::Display),
            Operation::MoveEdge(Side::West, Reach::Display),
            Operation::MoveEdge(Side::East, Reach::Display),
            Operation::Equalize,
            Operation::Balance,
            Operation::Manage,
            Operation::Stack(Side::West),
            Operation::Stack(Side::East),
            Operation::Unstack(Side::West),
            Operation::Unstack(Side::East),
            Operation::ToggleTabbedDisplay,
            Operation::Snap,
            Operation::Virtual(Direction::First),
            Operation::FocusOrVirtual(Direction::North),
            Operation::FocusOrVirtual(Direction::South),
            Operation::VirtualNumber(2),
            Operation::VirtualMove(Direction::East, MoveFocus::Follow),
            Operation::VirtualMove(Direction::East, MoveFocus::Stay),
            Operation::VirtualMoveNumber(0, MoveFocus::Follow),
            Operation::VirtualMoveNumber(0, MoveFocus::Stay),
            Operation::FocusUnmanaged,
            Operation::FocusManaged,
            Operation::RaiseFloating,
            Operation::ToggleFloatingLayer,
            Operation::CopyRule,
        ];

        for operation in operations {
            let command = Command::Window(operation.clone());
            let reparsed = round_trip(&command);
            assert_eq!(
                format!("{reparsed:?}"),
                format!("{command:?}"),
                "argv round-trip changed {operation:?}"
            );
        }
    }

    #[test]
    fn global_commands_round_trip() {
        for command in [
            Command::Quit,
            Command::Restart,
            Command::PrintState,
            Command::MenuBar(true),
            Command::MenuBar(false),
            Command::Mouse(MouseMove::ToNextDisplay),
        ] {
            assert_eq!(
                format!("{:?}", round_trip(&command)),
                format!("{command:?}")
            );
        }
    }

    #[test]
    fn lua_commands_have_no_argv_encoding() {
        assert!(Command::Lua(1).to_argv().is_none());
    }

    #[test]
    fn window_numbers_are_one_based() {
        assert_eq!(
            Direction::parse_positional("1"),
            Ok(Direction::Nth(0)),
            "the first window is number 1"
        );
        assert!(Direction::parse_positional("0").is_err());
    }

    #[test]
    fn stacking_without_a_side_keeps_its_original_direction() {
        assert_eq!(
            parse_command(&["window", "stack"]),
            Ok(Command::Window(Operation::Stack(Side::West)))
        );
        assert_eq!(
            parse_command(&["window", "unstack"]),
            Ok(Command::Window(Operation::Unstack(Side::East)))
        );
    }

    #[test]
    fn invalid_commands_are_rejected() {
        assert!(parse_command(&["definitely", "not", "a", "command"]).is_err());
        assert!(parse_command(&["window", "focus"]).is_err());
        assert!(parse_command(&["window", "swap", "3"]).is_err());
        assert!(parse_command(&["window", "stack", "north"]).is_err());
        assert!(parse_command(&["window", "unstack", "first"]).is_err());
        assert!(parse_command(&["showmenu", "extra"]).is_err());
        assert!(parse_command(&["hidemenu", "extra"]).is_err());
    }
}

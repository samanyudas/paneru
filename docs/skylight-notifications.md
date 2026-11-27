# SkyLight CGS Notification IDs

Reverse-engineered macOS SkyLight (`CGSRegisterNotifyProc`) notification IDs collected while developing Paneru.
`src/platform/notify.rs` registers only the subset Paneru acts on (`SpaceEvent`), while this table preserves the remaining IDs, discovery sources, and observed payload layouts.

## Credits

- `hs._asm.undocumented.spaces` (`CGSSpace.h`): https://github.com/asmagill/hs._asm.undocumented.spaces/blob/master/CGSSpace.h
- `yabai` (`src/misc/extern.h`): https://github.com/koekeishiya/yabai/blob/d55a647913ab72d8d8b348bee2d3e59e52ce4a5d/src/misc/extern.h
- `rift`: https://github.com/acsandmann/rift

## Notification table

| Name | ID | Notes and observed payload |
| --- | --- | --- |
| `DisplayWillSleep` | 102 | Display entering sleep |
| `DisplayDidWake` | 103 | Display waking from sleep |
| `WindowUpdated` | 723 | Window update |
| `kCGSEventNotificationSystemDefined` | 724 | Possibly loginwindow active |
| `WindowClosed` | 804 | Window ID (`WinID`) |
| `WindowMoved` | 806 | Window ID (`WinID`) |
| `WindowResized` | 807 | Window ID (`WinID`) |
| `WindowReordered` | 808 | Window ID (`WinID`) |
| `WindowLevelChanged` | 811 | Window ID (`WinID`) |
| `WindowUnhidden` | 815 | Window ID (`WinID`) |
| `WindowHidden` | 816 | Window ID (`WinID`) |
| `MissionControlEntered` | 1204 | Mission Control entered |
| `PackagesStatusBarSpaceChanged` | 1308 | Named in `_WSLogStringForNotifyType`; observed when the active display / status-bar space changes, including current-space and capability updates |
| `WindowTitleChanged` | 1322 | Window title changed |
| `SpaceWindowCreated` | 1325 | Window ID (`WinID`) |
| `SpaceWindowDestroyed` | 1326 | Registered by Paneru (`SpaceEvent::WindowDestroyed`); payload is `(u64 space, WinID window_id)` |
| `SpaceCreated` | 1327 | Registered by Paneru (`SpaceEvent::Created`) |
| `SpaceDestroyed` | 1328 | Registered by Paneru (`SpaceEvent::Destroyed`) |
| `SpaceCurrentChanged` | 1329 | Registered by Paneru (`SpaceEvent::CurrentChanged`); posted by `managed_display_set_current_space` through `post_space_lifecycle_notification`; likely carries the new current space id for a display transition |
| `WindowManagerActivatingClickOrdering` | 1333 | Local WM notification posted during activating-click ordering; payload is believed to be window/order metadata, exact layout under investigation |
| `WindowManagerSpaceFrontConnectionChanged` | 1334 | Local notification posted when the front connection for the current space changes |
| `WindowManagerGlobalFrontConnectionChanged` | 1335 | Local notification posted when the global front connection changes |
| `WindowOrderingGroupChanged` | 1336 | Posted from `finish_order_windows`; observed payload is `3 x u32` |
| `SpaceWindowTransactionCommitted` | 1338 | Posted by `-[PKGSpaceWindowManager_commitTransaction]`; useful as a transaction boundary even when per-window membership notifications race |
| `SpaceWindowBatchReassociated` | 1339 | Posted from `finishBatchReassociateWindows`; observed payload starts with a `u64` key/space followed by a `u32` count and repeated window ids |
| `SpaceWindowManagementCapabilitiesChanged` | 1340 | Posted via `__XSetSpaceWindowManagementCapabilities`; likely tied to space/window-management mode changes for a display or space |
| `WindowParentChanged` | 1341 | Posted from `_WSWindowSetParent` and related reassociation paths |
| `ManagedSpaceMembershipUpdated` | 1342 | Local notification from `managed_space_update_membership`; likely marks a completed space-membership mutation and may carry space/window ids |
| `WorkspaceWillChange` | 1400 | Workspace transition starting |
| `WorkspaceDidChange` | 1401 | Workspace transition finished |
| `WorkspaceWindowIsViewable` | 1402 | Window became viewable on workspace |
| `WorkspaceWindowIsNotViewable` | 1403 | Window no longer viewable on workspace |
| `WorkspaceWindowDidMove` | 1404 | Workspace window moved |
| `WorkspacePrefsDidChange` | 1405 | Workspace preferences changed |
| `WorkspacesWindowDragDidStart` | 1411 | Workspace window drag started |
| `WorkspacesWindowDragDidEnd` | 1412 | Workspace window drag ended |
| `WorkspacesWindowDragWillEnd` | 1413 | Workspace window drag ending |
| `WorkspacesShowSpaceForProcess` | 1414 | Show space for process |
| `WorkspacesWindowDidOrderInOnNonCurrentManagedSpacesOnly` | 1415 | Window ordered in on non-current managed spaces only |
| `WorkspacesWindowDidOrderOutOnNonCurrentManagedSpaces` | 1416 | Window ordered out on non-current managed spaces |
| `FrontmostApplicationChanged` | 1508 | Frontmost application changed |
| `TransitionDidFinish` | 1700 | Transition finished |
| `All` | `0xFFFF_FFFF` | Wildcard notification mask |

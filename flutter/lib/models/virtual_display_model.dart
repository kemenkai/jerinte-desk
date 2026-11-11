import 'dart:async';
import 'dart:convert';
import 'dart:math';
import 'dart:ui' as ui;

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:flutter_hbb/common.dart';
import 'package:flutter_hbb/consts.dart';
import 'package:flutter_hbb/models/model.dart';
import 'package:flutter_hbb/models/platform_model.dart';
import 'package:get/get.dart';
import 'package:window_size/window_size.dart' as window_size;

/// A monitor of this device, and the size of the virtual display it gets on the peer.
class LocalMonitor {
  final int slot;

  /// Where windows go to be on this monitor.
  final Rect rect;
  final double scale;
  final bool primary;
  final int width;
  final int height;

  LocalMonitor(this.slot, Map<String, dynamic> json)
      : rect = Rect.fromLTWH((json['x'] as num).toDouble(),
            (json['y'] as num).toDouble(), (json['w'] as num).toDouble(),
            (json['h'] as num).toDouble()),
        scale = (json['scale'] as num?)?.toDouble() ?? 1.0,
        primary = json['primary'] == true,
        width = json['width'] as int,
        height = json['height'] as int;
}

/// The virtual displays this session holds on the peer (src/vdisplay in Rust): one for each
/// monitor of this device that asked for one, shown on that monitor once the peer lists it.
/// Rust keeps what the session holds; every window of the session reads it from there.
class VirtualDisplayModel {
  final WeakReference<FFI> parent;
  VirtualDisplayModel(this.parent);

  /// This device's monitors; slot k is entry k.
  List<LocalMonitor> monitors = [];

  /// Slot -> the name the peer gave its display, empty until it answers.
  final displays = <int, String>{}.obs;

  // Slots this window shows once the peer lists their display.
  final _toShow = <int, _Pending>{};
  // Slots this window asked about, whose failures it reports.
  final _asked = <int>{};
  // The slot this window showed when the session went down, to show again once it is back.
  int? _showing;
  DateTime? _refreshedAt;

  // From the peer's answer. Longer than the peer may hold its display list back while it is
  // busy with the next request (`SETTLE_LIMIT` in src/vdisplay/mod.rs).
  static const _showTimeout = Duration(seconds: 45);
  // How long the view of this window stays the same before the display it shows follows it: a
  // window dragged to a size, or going fullscreen, passes through sizes not worth a resize of
  // the peer's display (a second or two each).
  static const _viewSettle = Duration(seconds: 1);
  Timer? _viewTimer;
  // What this window last told Rust: the slot it shows, and its view. Told again only when that
  // changes here, so a display shown in two windows follows the one last resized or switched to
  // it, rather than each of them in turn, without end.
  String? _toldView;

  Map<String, dynamic>? get _caps => parent.target?.ffiModel.pi
      .platformAdditions[kPlatformAdditionsVirtualDisplayCaps];

  /// Whether the peer offers virtual displays to this session. Desktop only for now.
  bool get available {
    final ffi = parent.target;
    return isDesktop &&
        ffi != null &&
        ffi.connType == ConnType.defaultConn &&
        _caps != null;
  }

  bool get peerHasHiDpi => _caps?['hidpi'] == true;

  /// The slot of the display `pi` shows, if it is one this session holds.
  int? shownSlot(PeerInfo pi) {
    final i = pi.currentDisplay;
    if (i < 0 || i >= pi.displays.length) return null;
    final name = pi.displays[i].name;
    if (name.isEmpty) return null;
    for (final e in displays.entries) {
      if (e.value == name) return e.key;
    }
    return null;
  }

  /// Whether the display `pi` shows is one this session holds, made at the size in points of the
  /// view here: the peer has no HiDPI displays, and all the pixels were not asked for
  /// (src/vdisplay/controller.rs `spec_for_view`).
  bool showsOneInPoints(PeerInfo pi) {
    final ffi = parent.target;
    if (ffi == null || peerHasHiDpi) return false;
    if (bind.sessionGetToggleOptionSync(
        sessionId: ffi.sessionId, arg: kOptionVirtualDisplayPixels)) {
      return false;
    }
    return shownSlot(pi) != null;
  }

  /// The view of this window may have changed, or the display it shows: once the view settles,
  /// a display of this session shown here takes its size (Rust asks the peer only for a size the
  /// display does not have, one request at a time).
  void followView() {
    if (!available) return;
    _viewTimer?.cancel();
    _viewTimer = Timer(_viewSettle, () {
      final ffi = parent.target;
      if (ffi == null) return;
      final slot = shownSlot(ffi.ffiModel.pi);
      if (slot != null) _reportView(slot);
    });
  }

  // Tells Rust the view of this window shows `slot`, unless it was the last thing told; `always`
  // for a display about to be asked for, which Rust sizes by it.
  void _reportView(int slot, {bool always = false}) {
    final ffi = parent.target;
    if (ffi == null) return;
    final size = ffi.canvasModel.getSize();
    final views = ui.PlatformDispatcher.instance.views;
    // Minimized, or not laid out yet.
    if (size.width < 1 || size.height < 1 || views.isEmpty) return;
    final told =
        '$slot ${size.width} ${size.height} ${views.first.devicePixelRatio}';
    if (!always && told == _toldView) return;
    _toldView = told;
    bind.sessionSetVirtualDisplayView(
        sessionId: ffi.sessionId,
        slot: slot,
        width: size.width,
        height: size.height,
        scale: views.first.devicePixelRatio);
  }

  /// The modes the peer offers for a virtual display (src/vdisplay caps `modes`). The shared
  /// screen is always one; some peers also offer "privacy", the desktop taken over on the
  /// display with the peer's monitors dark, and "nested", a session of its own on it.
  List<String> get peerModes {
    final m = _caps?['modes'];
    return m is List ? m.map((e) => e.toString()).toList() : const ['shared'];
  }

  bool get peerHasPrivacy => peerModes.contains('privacy');

  /// The mode new virtual displays of this session are asked for in — "shared" (an extra screen),
  /// "privacy" or "nested". Read from the peer options once the session is up, kept here so the
  /// toolbar reads it without an async call.
  final _mode = 'shared'.obs;
  String get mode => _mode.value;
  // Counts the changes of `_mode`, so a read of the option overtaken by a newer one is dropped.
  int _modeChanges = 0;
  // The last write of the option: a read waits for it, lest it run first and read what it replaced.
  Future<void> _modeWritten = Future.value();

  Future<void> _loadMode() async {
    final ffi = parent.target;
    if (ffi == null) return;
    final change = ++_modeChanges;
    await _modeWritten;
    final v = await bind.sessionGetOption(
            sessionId: ffi.sessionId, arg: kOptionVirtualDisplayMode) ??
        '';
    if (change != _modeChanges) return;
    _mode.value = v.isEmpty ? 'shared' : v;
  }

  /// Sets the mode and re-asks the held displays in it, so the change takes effect at once.
  Future<void> setMode(String mode) async {
    final ffi = parent.target;
    if (ffi == null) return;
    _modeChanges++;
    _mode.value = mode;
    await (_modeWritten = bind.sessionPeerOption(
        sessionId: ffi.sessionId,
        name: kOptionVirtualDisplayMode,
        value: mode == 'shared' ? '' : mode));
    resizeHeld();
  }

  /// Reads this device's monitors and what the session holds from Rust.
  void refresh() {
    final ffi = parent.target;
    if (ffi == null) return;
    _refreshedAt = DateTime.now();
    try {
      final state = jsonDecode(
              bind.sessionGetVirtualDisplays(sessionId: ffi.sessionId))
          as Map<String, dynamic>;
      final list = state['monitors'] as List<dynamic>;
      monitors = [
        for (var i = 0; i < list.length; i++) LocalMonitor(i, list[i])
      ];
      final held = (state['held'] as Map<String, dynamic>)
          .map((slot, name) => MapEntry(int.parse(slot), name as String));
      if (!mapEquals(held, displays)) {
        displays.assignAll(held);
        // The display shown may have become one of this session's, shown otherwise at its
        // original size.
        ffi.canvasModel.updateViewStyle();
      }
    } catch (e) {
      debugPrint('Failed to read the virtual displays: $e');
    }
  }

  /// For a build: reads Rust again after the frame, at most once a second.
  void refreshSoon() {
    final at = _refreshedAt;
    if (at != null &&
        DateTime.now().difference(at) < const Duration(seconds: 1)) {
      return;
    }
    _refreshedAt = DateTime.now();
    WidgetsBinding.instance.addPostFrameCallback((_) => refresh());
  }

  Future<void> toggle(int slot, bool on) async {
    final ffi = parent.target;
    if (ffi == null) return;
    // To be shown in this window (`_show`): the size of its view from the start, rather than the
    // monitor's first and its view's a moment later.
    if (on && slot == await _monitorOfThisWindow()) {
      _reportView(slot, always: true);
    }
    _asked.add(slot);
    _toShow.remove(slot)?.timer?.cancel();
    if (on) {
      _toShow[slot] = _Pending();
    } else {
      _leaveIfShowing([slot]);
    }
    bind.sessionToggleVirtualDisplaySlot(
        sessionId: ffi.sessionId, slot: slot, on: on);
    refresh();
  }

  void removeAll() {
    final ffi = parent.target;
    if (ffi == null) return;
    // Answered once, for all.
    _asked.add(kAllVirtualDisplay);
    for (final pending in _toShow.values) {
      pending.timer?.cancel();
    }
    _toShow.clear();
    _leaveIfShowing(displays.keys.toList());
    bind.sessionToggleVirtualDisplaySlot(
        sessionId: ffi.sessionId, slot: kAllVirtualDisplay, on: false);
    refresh();
  }

  /// Asks for every held display again at the size it now gets (its view's, else its monitor's),
  /// after a setting that changes it.
  void resizeHeld() {
    final ffi = parent.target;
    if (ffi == null) return;
    for (final slot in displays.keys.toList()) {
      // Asked here: what the peer did not do as asked is told here.
      _asked.add(slot);
      bind.sessionToggleVirtualDisplaySlot(
          sessionId: ffi.sessionId, slot: slot, on: true);
    }
  }

  /// Before a login replaces the display list: which of ours this window shows.
  void beforePeerInfo() {
    final current =
        parent.target?.ffiModel.pi.tryGetDisplayIfNotAllDisplay();
    _showing = null;
    if (current == null || current.name.isEmpty) return;
    for (final e in displays.entries) {
      if (e.value == current.name) _showing = e.key;
    }
  }

  /// After every login, in every window. Once per session, where turned on for this peer
  /// (`kOptionAutoVirtualDisplay`): a virtual display the size of the monitor this window is on.
  Future<void> onPeerInfo(bool isCache) async {
    refresh();
    if (!available) return;
    final ffi = parent.target!;
    _loadMode();
    final showing = _showing;
    _showing = null;
    if (!isCache && showing != null && displays.containsKey(showing)) {
      // The session asks for it again on reconnecting; back on it once it is there, in this
      // window wherever it is.
      _toShow.remove(showing)?.timer?.cancel();
      _toShow[showing] = _Pending(here: true);
      _showListed();
    }
    if (!bind.sessionTakeAutoVirtualDisplay(sessionId: ffi.sessionId)) return;
    if (ffi.ffiModel.viewOnly || !ffi.ffiModel.keyboard) return;
    // Only for a peer the user asked it for.
    if (!bind.sessionGetToggleOptionSync(
        sessionId: ffi.sessionId, arg: kOptionAutoVirtualDisplay)) {
      return;
    }
    // The user maps the peer's own displays onto these monitors instead.
    if (bind.sessionGetUseAllMyDisplaysForTheRemoteSession(
            sessionId: ffi.sessionId) ==
        'Y') {
      return;
    }
    if (_caps?['free'] == 0 || monitors.isEmpty) return;
    toggle(await _monitorOfThisWindow(), true);
  }

  /// The peer's answer for a slot: what it holds now, and why a request failed if it did.
  void onResult(Map<String, dynamic> evt) {
    final slot = int.tryParse(evt['slot'] ?? '');
    if (slot == null) return;
    // The display not used as the session asked, the mode it asks in is set back (Rust, which
    // has told the user why). Read again rather than taken from the event: a choice made since
    // the event was sent is newer.
    final setBack = evt['mode'] != null;
    if (setBack) {
      _loadMode();
    }
    if (evt['notice'] == 'true') {
      // Word from the peer, answering no request: the display is an extra screen now.
      refresh();
      return;
    }
    final String error = evt['error'] ?? '';
    final mine = _asked.remove(slot);
    refresh();
    final pending = _toShow[slot];
    if (!displays.containsKey(slot)) {
      _toShow.remove(slot)?.timer?.cancel();
    } else if (pending != null && evt['on'] == 'true') {
      // From here, the display has a while to show up.
      pending.timer?.cancel();
      pending.timer = Timer(_showTimeout, () {
        if (identical(_toShow[slot], pending)) {
          _toShow.remove(slot);
          showToast(translate('The virtual display did not show up'));
        }
      });
    }
    if (error.isNotEmpty && mine && !setBack) {
      showToast(translate(error));
    }
    _showListed();
  }

  /// The peer's display list changed; [shown] is the name of the display this window showed until
  /// then, null for all of them. One of ours stays shown here wherever it is listed now: the index
  /// alone may be another display's, as when the peer takes its desktop back and lists its own
  /// monitors ahead of ours.
  void onDisplaysChanged(String? shown) {
    final ffi = parent.target;
    if (ffi == null) return;
    final pi = ffi.ffiModel.pi;
    final listed = pi.displays.map((d) => d.name).toList();
    final slot = slotToFollow(displays, shown, listed, pi.currentDisplay);
    // One waiting already (as from `onPeerInfo` on reconnecting) is shown as it was asked for.
    if (slot != null && !_toShow.containsKey(slot)) {
      _toShow[slot] = _Pending(here: true, keepCursor: true);
    }
    _showListed();
    followView();
  }

  void _showListed() {
    final ffi = parent.target;
    if (ffi == null) return;
    for (final slot in _toShow.keys.toList()) {
      final name = displays[slot];
      if (name == null || name.isEmpty) continue;
      final index = ffi.ffiModel.pi.displays.indexWhere((d) => d.name == name);
      if (index < 0) continue;
      final pending = _toShow.remove(slot)!;
      pending.timer?.cancel();
      _show(slot, index, here: pending.here, keepCursor: pending.keepCursor);
    }
  }

  // On the monitor it was made for: this window if it is there, a new one if not. With [here],
  // in this window wherever it is, switched to before any await.
  Future<void> _show(int slot, int index,
      {bool here = false, bool keepCursor = false}) async {
    final ffi = parent.target;
    if (ffi == null) return;
    if (!here &&
        slot < monitors.length &&
        slot != await _monitorOfThisWindow()) {
      openMonitorInNewTabOrWindow(index, ffi.id, ffi.ffiModel.pi,
          screenRect: monitors[slot].rect);
    } else {
      openMonitorInTheSameTab(index, ffi, ffi.ffiModel.pi,
          updateCursorPos: !keepCursor);
    }
  }

  // Moves this window off a virtual display about to go, to the first physical display.
  void _leaveIfShowing(List<int> slots) {
    final ffi = parent.target;
    if (ffi == null) return;
    final pi = ffi.ffiModel.pi;
    final current = pi.tryGetDisplayIfNotAllDisplay();
    final going = slots.map((s) => displays[s]).whereType<String>().toSet();
    if (current == null || !going.contains(current.name)) return;
    final physical =
        pi.displays.indexWhere((d) => !displays.values.contains(d.name));
    openMonitorInTheSameTab(physical < 0 ? 0 : physical, ffi, pi);
  }

  // The monitor this window is on, else the primary one.
  Future<int> _monitorOfThisWindow() async {
    try {
      final screen = (await window_size.getWindowInfo()).screen;
      final slot = screen == null
          ? null
          : monitorOfScreen(monitors, screen.frame, yFromTopmost: isMacOS);
      if (slot != null) return slot;
    } catch (e) {
      debugPrint('Failed to find the monitor of this window: $e');
    }
    final primary = monitors.indexWhere((m) => m.primary);
    return primary < 0 ? 0 : primary;
  }
}

// A display this window shows once the peer lists it.
class _Pending {
  // In this window wherever it is, rather than on the monitor the display was made for.
  final bool here;
  // Shown here already, only listed at another index now: the peer's cursor, which the person
  // there may be using, stays where it is.
  final bool keepCursor;
  // Runs from the peer's answer.
  Timer? timer;
  _Pending({this.here = false, this.keepCursor = false});
}

/// The slot of the monitor that [screen], a `window_size` screen frame, is. Matched on x and
/// size, which every platform reports as the monitors do; y only breaks ties between monitors
/// stacked one above the other. On macOS that y counts from the top of the topmost monitor
/// ([yFromTopmost]), not from the primary one's as the monitors' does.
int? monitorOfScreen(List<LocalMonitor> monitors, Rect screen,
    {bool yFromTopmost = false}) {
  if (monitors.isEmpty) return null;
  final top = yFromTopmost ? monitors.map((m) => m.rect.top).reduce(min) : 0.0;
  int? best;
  var bestDy = double.infinity;
  for (final m in monitors) {
    if ((m.rect.left - screen.left).abs() <= 1 &&
        (m.rect.width - screen.width).abs() <= 1 &&
        (m.rect.height - screen.height).abs() <= 1) {
      final dy = (m.rect.top - top - screen.top).abs();
      if (dy < bestDy) {
        best = m.slot;
        bestDy = dy;
      }
    }
  }
  return best;
}

/// The slot of [shown], the name of the display a window shows (null for all of them), if it is
/// one of [held] (slot -> name) that [listed], the names of the peer's displays, now has at an
/// index other than [current], the window's. A display list from the peer keeps the index a
/// window shows, which may be another display's now: one of ours is followed by name instead.
/// One without a name is not.
int? slotToFollow(
    Map<int, String> held, String? shown, List<String> listed, int current) {
  if (shown == null || shown.isEmpty) return null;
  final index = listed.indexOf(shown);
  if (index < 0 || index == current) return null;
  for (final e in held.entries) {
    if (e.value == shown) return e.key;
  }
  return null;
}

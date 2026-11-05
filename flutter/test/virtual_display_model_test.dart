import 'dart:ui';

import 'package:flutter_hbb/consts.dart';
import 'package:flutter_hbb/models/virtual_display_model.dart';
import 'package:flutter_test/flutter_test.dart';

LocalMonitor _monitor(int slot, double x, double y, double w, double h,
        {bool primary = false}) =>
    LocalMonitor(slot, {
      'x': x,
      'y': y,
      'w': w,
      'h': h,
      'scale': 1.0,
      'primary': primary,
      'width': w.toInt(),
      'height': h.toInt(),
    });

void main() {
  test('a monitor list from Rust parses', () {
    final m = LocalMonitor(1, {
      'x': -1440,
      'y': 0,
      'w': 1440,
      'h': 900,
      'scale': 2.0,
      'primary': false,
      'width': 1440,
      'height': 900,
    });
    expect(m.slot, 1);
    expect(m.rect, const Rect.fromLTWH(-1440, 0, 1440, 900));
    expect(m.scale, 2.0);
    expect(m.primary, isFalse);
    expect((m.width, m.height), (1440, 900));
  });

  test('the screen of a window is found by x and size', () {
    final monitors = [
      _monitor(0, 0, 0, 1440, 900, primary: true),
      _monitor(1, 1440, -540, 2560, 1440),
    ];
    // macOS reports y flipped about the topmost screen, so only x and size can be trusted.
    expect(monitorOfScreen(monitors, const Rect.fromLTWH(1440, 0, 2560, 1440)), 1);
    expect(monitorOfScreen(monitors, const Rect.fromLTWH(0, 540, 1440, 900)), 0);
    // A screen that is not in the list.
    expect(monitorOfScreen(monitors, const Rect.fromLTWH(0, 0, 1920, 1080)), isNull);
  });

  test('stacked monitors of one size are told apart by y', () {
    final monitors = [
      _monitor(0, 0, 0, 1920, 1080, primary: true),
      _monitor(1, 0, -1080, 1920, 1080),
    ];
    // Windows and X11 report y as the monitors do.
    expect(monitorOfScreen(monitors, const Rect.fromLTWH(0, -1080, 1920, 1080)), 1);
    expect(monitorOfScreen(monitors, const Rect.fromLTWH(0, 0, 1920, 1080)), 0);
    // macOS counts y from the top of the topmost monitor, here the one above the primary.
    expect(
        monitorOfScreen(monitors, const Rect.fromLTWH(0, 0, 1920, 1080),
            yFromTopmost: true),
        1);
    expect(
        monitorOfScreen(monitors, const Rect.fromLTWH(0, 1080, 1920, 1080),
            yFromTopmost: true),
        0);
  });

  test('no monitors, no match', () {
    expect(monitorOfScreen([], const Rect.fromLTWH(0, 0, 1920, 1080)), isNull);
  });

  // One of the peer's own monitors, and virtual displays this session holds for this device's
  // monitors (slots) 2 and 0, neither of them an index the display moves from or to below.
  const p = 'HDMI-1', v = 'Virtual-1', v2 = 'Virtual-2';
  const held = {2: v};

  test('a virtual display is followed as the peer takes its desktop back', () {
    // [V] -> [P, V], the window on 0.
    expect(slotToFollow(held, v, [p, v], 0), 2);
  });

  test('a virtual display is followed as the desktop is taken over', () {
    // [P, V] -> [V], the window on 1.
    expect(slotToFollow(held, v, [v], 1), 2);
  });

  test('a virtual display where it was stays', () {
    expect(slotToFollow(held, v, [v, p], 0), isNull);
  });

  test('a virtual display gone is not followed', () {
    expect(slotToFollow(held, v, [p], 0), isNull);
  });

  test('only a virtual display of this session with a name is followed', () {
    // The peer's own, moved.
    expect(slotToFollow(held, p, [v, p], 0), isNull);
    // All displays in one view.
    expect(slotToFollow(held, null, [p, v], kAllDisplayValue), isNull);
    // One the peer has not named yet.
    expect(slotToFollow({2: v, 0: ''}, '', [p, ''], 0), isNull);
  });

  test('of two virtual displays, the one shown is followed', () {
    const two = {2: v, 0: v2};
    // [V, V2] -> [P, V, V2].
    expect(slotToFollow(two, v2, [p, v, v2], 1), 0);
    expect(slotToFollow(two, v, [p, v, v2], 0), 2);
  });
}

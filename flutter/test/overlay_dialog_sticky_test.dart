import 'package:flutter/material.dart';
import 'package:flutter_hbb/common.dart';
import 'package:flutter_test/flutter_test.dart';

// The remote page's overlay, as it builds one anew on a change of its state.
Widget page(OverlayKeyState keyState) => MaterialApp(
      home: Overlay(
          key: keyState.key,
          initialEntries: [OverlayEntry(builder: (_) => const SizedBox())]),
    );

// `CustomAlertDialog` asks for focus on a timer at each build: run before the test ends.
Future<void> settle(WidgetTester tester) =>
    tester.pump(const Duration(milliseconds: 10));

DialogBuilder dialog(String text) => (setState, close, context) =>
    CustomAlertDialog(
      content: Text(text),
      actions: [TextButton(onPressed: () => close(), child: Text('OK $text'))],
    );

// As `RemotePage` builds its body once the image is ready: clean up, and a new overlay each time.
class RebuildingPage extends StatefulWidget {
  final OverlayDialogManager dialogs;
  final ValueNotifier<int> rebuild;
  const RebuildingPage(this.dialogs, this.rebuild, {super.key});

  @override
  State<RebuildingPage> createState() => _RebuildingPageState();
}

class _RebuildingPageState extends State<RebuildingPage> {
  @override
  Widget build(BuildContext context) => ValueListenableBuilder<int>(
      valueListenable: widget.rebuild,
      builder: (context, _, __) {
        widget.dialogs.dismissTransient();
        final keyState = OverlayKeyState();
        widget.dialogs.setOverlayState(keyState);
        return Overlay(
            key: keyState.key,
            initialEntries: [OverlayEntry(builder: (_) => const SizedBox())]);
      });
}

void main() {
  testWidgets('a sticky dialog stays through the clean-up and moves to a new overlay',
      (tester) async {
    final dialogs = OverlayDialogManager();
    final first = OverlayKeyState();
    dialogs.setOverlayState(first);
    await tester.pumpWidget(page(first));
    var answered = false;
    dialogs.show(dialog('stay'), tag: 'stay', sticky: true).then((_) => answered = true);
    dialogs.show(dialog('go'), tag: 'go');
    await tester.pump();
    expect(find.text('stay'), findsOneWidget);
    expect(find.text('go'), findsOneWidget);

    // What the remote page does once its image is ready: clean up, then a new overlay.
    dialogs.dismissTransient();
    final second = OverlayKeyState();
    dialogs.setOverlayState(second);
    await tester.pumpWidget(page(second));
    await tester.pump();
    expect(find.text('stay'), findsOneWidget);
    expect(find.text('go'), findsNothing);
    expect(answered, isFalse);

    // Still answerable where it is now; once answered it moves no more.
    await tester.tap(find.text('OK stay'));
    await tester.pump();
    expect(find.text('stay'), findsNothing);
    expect(answered, isTrue);
    final third = OverlayKeyState();
    dialogs.setOverlayState(third);
    await tester.pumpWidget(page(third));
    await tester.pump();
    expect(find.text('stay'), findsNothing);
    await settle(tester);
  });

  testWidgets('dismissing all dismisses a sticky dialog too', (tester) async {
    final dialogs = OverlayDialogManager();
    final first = OverlayKeyState();
    dialogs.setOverlayState(first);
    await tester.pumpWidget(page(first));
    dialogs.show(dialog('stay'), tag: 'stay', sticky: true);
    await tester.pump();
    dialogs.dismissAll();
    final second = OverlayKeyState();
    dialogs.setOverlayState(second);
    await tester.pumpWidget(page(second));
    await tester.pump();
    expect(find.text('stay'), findsNothing);

    // So does dismissing it by its tag.
    dialogs.show(dialog('again'), tag: 'again', sticky: true);
    await tester.pump();
    dialogs.dismissByTag('again');
    final third = OverlayKeyState();
    dialogs.setOverlayState(third);
    await tester.pumpWidget(page(third));
    await tester.pump();
    expect(find.text('again'), findsNothing);
    await settle(tester);
  });

  testWidgets('a sticky dialog stays while the page builds its overlay anew',
      (tester) async {
    final dialogs = OverlayDialogManager();
    final rebuild = ValueNotifier(0);
    await tester.pumpWidget(MaterialApp(home: RebuildingPage(dialogs, rebuild)));
    dialogs.show(dialog('stay'), tag: 'stay', sticky: true);
    await tester.pump();
    for (var i = 1; i <= 3; i++) {
      rebuild.value = i;
      await tester.pump();
      await tester.pump();
      expect(find.text('stay'), findsOneWidget);
    }
    await tester.tap(find.text('OK stay'));
    await tester.pump();
    rebuild.value = 4;
    await tester.pump();
    await tester.pump();
    expect(find.text('stay'), findsNothing);
    await settle(tester);
  });

  testWidgets('a tag used again by a dialog not sticky is not sticky', (tester) async {
    final dialogs = OverlayDialogManager();
    final first = OverlayKeyState();
    dialogs.setOverlayState(first);
    await tester.pumpWidget(page(first));
    dialogs.show(dialog('old'), tag: 'tag', sticky: true);
    dialogs.show(dialog('new'), tag: 'tag');
    await tester.pump();
    dialogs.dismissTransient();
    final second = OverlayKeyState();
    dialogs.setOverlayState(second);
    await tester.pumpWidget(page(second));
    await tester.pump();
    expect(find.text('new'), findsNothing);
    expect(find.text('old'), findsNothing);
    await settle(tester);
  });

  testWidgets('a sticky dialog shown before the page is there moves onto it', (tester) async {
    final dialogs = OverlayDialogManager();
    // The page's overlay not in the tree yet: the dialog goes on the global one.
    dialogs.setOverlayState(OverlayKeyState());
    final pageOverlay = OverlayKeyState();
    final showPage = ValueNotifier(false);
    await tester.pumpWidget(MaterialApp(
        navigatorKey: globalKey,
        home: ValueListenableBuilder<bool>(
            valueListenable: showPage,
            builder: (_, shown, __) => shown
                ? Overlay(
                    key: pageOverlay.key,
                    initialEntries: [OverlayEntry(builder: (_) => const SizedBox())])
                : const SizedBox())));
    dialogs.show(dialog('stay'), tag: 'stay', sticky: true);
    await tester.pump();
    expect(find.text('stay'), findsOneWidget);
    showPage.value = true;
    dialogs.setOverlayState(pageOverlay);
    await tester.pump();
    await tester.pump();
    expect(find.text('stay'), findsOneWidget);
    expect(
        find.descendant(of: find.byKey(pageOverlay.key!), matching: find.text('stay')),
        findsOneWidget);
    await settle(tester);
  });

  testWidgets('a dialog not sticky goes with the clean-up', (tester) async {
    final dialogs = OverlayDialogManager();
    final first = OverlayKeyState();
    dialogs.setOverlayState(first);
    await tester.pumpWidget(page(first));
    var done = false;
    dialogs.show(dialog('go'), tag: 'go').then((_) => done = true);
    await tester.pump();
    dialogs.dismissTransient();
    await tester.pump();
    expect(find.text('go'), findsNothing);
    expect(done, isTrue);
    await settle(tester);
  });
}

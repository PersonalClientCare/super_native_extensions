import 'package:flutter/gestures.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:super_drag_and_drop/src/drag_internal.dart';

const _delay = Duration(milliseconds: 300);

void main() {
  late int dragStarts;
  late int panUpdates;

  setUp(() {
    dragStarts = 0;
    panUpdates = 0;
  });

  Future<void> pumpDetector(
    WidgetTester tester, {
    Duration? mouseDragDelay,
    bool draggable = true,
    bool withPan = false,
  }) {
    return tester.pumpWidget(
      Directionality(
        textDirection: TextDirection.ltr,
        child: RawGestureDetector(
          behavior: HitTestBehavior.opaque,
          gestures: {
            DesktopMultiDragGestureRecognizer:
                GestureRecognizerFactoryWithHandlers<
                        DesktopMultiDragGestureRecognizer>(
                    () => DesktopMultiDragGestureRecognizer(
                        isLocationDraggable: (_) => draggable,
                        mouseDragDelay: mouseDragDelay), (recognizer) {
              recognizer.mouseDragDelay = mouseDragDelay;
              recognizer.onStart = (_) {
                dragStarts++;
                return _NoopDrag();
              };
            }),
            // Competes in the arena, so it only joins where a test needs it.
            if (withPan)
              PanGestureRecognizer:
                  GestureRecognizerFactoryWithHandlers<PanGestureRecognizer>(
                      () => PanGestureRecognizer(), (recognizer) {
                recognizer.onUpdate = (_) => panUpdates++;
              }),
          },
          child: const SizedBox.expand(),
        ),
      ),
    );
  }

  testWidgets("mouse drag starts on movement without a delay", (tester) async {
    await pumpDetector(tester);
    final gesture = await tester.startGesture(
      const Offset(100, 100),
      kind: PointerDeviceKind.mouse,
    );
    await gesture.moveBy(const Offset(20, 0));
    await tester.pump();
    expect(dragStarts, 1);
    await gesture.up();
  });

  testWidgets("mouse drag starts once the delay elapsed while held still",
      (tester) async {
    await pumpDetector(tester, mouseDragDelay: _delay);
    final gesture = await tester.startGesture(
      const Offset(100, 100),
      kind: PointerDeviceKind.mouse,
    );
    await tester.pump(_delay - const Duration(milliseconds: 50));
    expect(dragStarts, 0);
    await tester.pump(const Duration(milliseconds: 100));
    expect(dragStarts, 1);
    await gesture.up();
  });

  testWidgets("moving before the delay rejects the drag so a pan wins",
      (tester) async {
    await pumpDetector(tester, mouseDragDelay: _delay, withPan: true);
    final gesture = await tester.startGesture(
      const Offset(100, 100),
      kind: PointerDeviceKind.mouse,
    );
    await gesture.moveBy(const Offset(30, 0));
    await tester.pump();
    await gesture.moveBy(const Offset(30, 0));
    await tester.pump(_delay * 2);
    expect(dragStarts, 0);
    expect(panUpdates, greaterThan(0));
    await gesture.up();
  });

  testWidgets("releasing before the delay never starts a drag", (tester) async {
    await pumpDetector(tester, mouseDragDelay: _delay);
    final gesture = await tester.startGesture(
      const Offset(100, 100),
      kind: PointerDeviceKind.mouse,
    );
    await tester.pump(const Duration(milliseconds: 100));
    await gesture.up();
    await tester.pump(_delay * 2);
    expect(dragStarts, 0);
  });

  testWidgets("a zero delay behaves like no delay", (tester) async {
    await pumpDetector(tester, mouseDragDelay: Duration.zero);
    final gesture = await tester.startGesture(
      const Offset(100, 100),
      kind: PointerDeviceKind.mouse,
    );
    await gesture.moveBy(const Offset(20, 0));
    await tester.pump();
    expect(dragStarts, 1);
    await gesture.up();
  });

  testWidgets("the delay does not apply to touch pointers", (tester) async {
    await pumpDetector(tester, mouseDragDelay: _delay);
    final gesture = await tester.startGesture(const Offset(100, 100));
    await gesture.moveBy(const Offset(40, 0));
    await tester.pump();
    expect(dragStarts, 1);
    await gesture.up();
  });

  testWidgets("a location that is not draggable never starts a drag",
      (tester) async {
    await pumpDetector(tester, mouseDragDelay: _delay, draggable: false);
    final gesture = await tester.startGesture(
      const Offset(100, 100),
      kind: PointerDeviceKind.mouse,
    );
    await tester.pump(_delay * 2);
    expect(dragStarts, 0);
    await gesture.up();
  });
}

class _NoopDrag extends Drag {}

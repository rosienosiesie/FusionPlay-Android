import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:fusionplay_android_flutter/models/app_state.dart';
import 'package:fusionplay_android_flutter/ui/animated_press_scale.dart';
import 'package:fusionplay_android_flutter/ui/artwork.dart';
import 'package:fusionplay_android_flutter/ui/player_view.dart';
import 'package:fusionplay_android_flutter/ui/remote_focus.dart';
import 'package:fusionplay_android_flutter/ui/rounded_progress_bar.dart';

void main() {
  testWidgets('D-pad moves one focus at a time and select activates it', (
    tester,
  ) async {
    final first = FocusNode(debugLabel: 'first');
    final second = FocusNode(debugLabel: 'second');
    var activated = false;
    addTearDown(() {
      first.dispose();
      second.dispose();
    });

    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: Row(
            children: [
              RemoteFocusFrame(
                autofocus: true,
                focusNode: first,
                onActivate: () {},
                child: const SizedBox.square(dimension: 48),
              ),
              const SizedBox(width: 24),
              RemoteFocusFrame(
                focusNode: second,
                onActivate: () => activated = true,
                child: const SizedBox.square(dimension: 48),
              ),
            ],
          ),
        ),
      ),
    );
    await tester.pump();

    expect(first.hasPrimaryFocus, isTrue);
    await tester.sendKeyEvent(LogicalKeyboardKey.arrowRight);
    await tester.pump();
    expect(second.hasPrimaryFocus, isTrue);

    await tester.sendKeyEvent(LogicalKeyboardKey.select);
    await tester.pump();
    expect(activated, isTrue);
  });

  testWidgets('disabled controls are skipped by D-pad traversal', (
    tester,
  ) async {
    final first = FocusNode(debugLabel: 'first');
    final disabled = FocusNode(debugLabel: 'disabled');
    final last = FocusNode(debugLabel: 'last');
    addTearDown(() {
      first.dispose();
      disabled.dispose();
      last.dispose();
    });

    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: Row(
            children: [
              RemoteFocusFrame(
                autofocus: true,
                focusNode: first,
                onActivate: () {},
                child: const SizedBox.square(dimension: 48),
              ),
              RemoteFocusFrame(
                enabled: false,
                focusNode: disabled,
                child: const SizedBox.square(dimension: 48),
              ),
              RemoteFocusFrame(
                focusNode: last,
                onActivate: () {},
                child: const SizedBox.square(dimension: 48),
              ),
            ],
          ),
        ),
      ),
    );
    await tester.pump();

    await tester.sendKeyEvent(LogicalKeyboardKey.arrowRight);
    await tester.pump();
    expect(disabled.hasFocus, isFalse);
    expect(last.hasPrimaryFocus, isTrue);
  });

  testWidgets('focused progress bar consumes left and right as seek steps', (
    tester,
  ) async {
    var changed = .5;
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: Row(
            children: [
              RemoteFocusFrame(
                autofocus: true,
                onActivate: () {},
                child: const SizedBox.square(dimension: 48),
              ),
              SizedBox(
                width: 240,
                child: RoundedProgressBar(
                  value: .5,
                  enabled: true,
                  seekEnabled: true,
                  activeColor: Colors.black,
                  inactiveColor: Colors.grey,
                  onChanged: (value) => changed = value,
                ),
              ),
            ],
          ),
        ),
      ),
    );
    await tester.pump();

    await tester.sendKeyEvent(LogicalKeyboardKey.arrowRight);
    await tester.pump();
    await tester.sendKeyEvent(LogicalKeyboardKey.arrowRight);
    await tester.pump();
    expect(changed, closeTo(.52, .001));
  });

  testWidgets('touch drag shares the progress focus highlight', (tester) async {
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: Center(
            child: SizedBox(
              width: 240,
              child: RoundedProgressBar(
                value: .5,
                enabled: true,
                seekEnabled: true,
                showThumb: false,
                activeColor: Colors.black,
                inactiveColor: Colors.grey,
                onChanged: (_) {},
              ),
            ),
          ),
        ),
      ),
    );

    final animationFinder = find.descendant(
      of: find.byType(RoundedProgressBar),
      matching: find.byWidgetPredicate(
        (widget) => widget is TweenAnimationBuilder<double>,
      ),
    );
    double highlightTarget() => tester
        .widget<TweenAnimationBuilder<double>>(animationFinder)
        .tween
        .end!;

    expect(highlightTarget(), 0);
    final gesture = await tester.startGesture(
      tester.getCenter(find.byType(RoundedProgressBar)),
    );
    await tester.pump();
    expect(highlightTarget(), 1);

    await gesture.moveBy(const Offset(40, 0));
    await tester.pump();
    expect(highlightTarget(), 1);

    await gesture.up();
    await tester.pump();
    expect(highlightTarget(), 0);
  });

  testWidgets('progress up and down use explicit spatial targets', (
    tester,
  ) async {
    final progress = FocusNode(debugLabel: 'progress');
    final settings = FocusNode(debugLabel: 'settings');
    final playPause = FocusNode(debugLabel: 'play-pause');
    addTearDown(progress.dispose);
    addTearDown(settings.dispose);
    addTearDown(playPause.dispose);

    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: Column(
            children: [
              RemoteFocusFrame(
                focusNode: settings,
                onActivate: () {},
                child: const SizedBox.square(dimension: 48),
              ),
              SizedBox(
                width: 240,
                child: RoundedProgressBar(
                  value: .5,
                  enabled: true,
                  seekEnabled: true,
                  showThumb: false,
                  focusNode: progress,
                  upFocusNode: settings,
                  downFocusNode: playPause,
                  activeColor: Colors.black,
                  inactiveColor: Colors.grey,
                  onChanged: (_) {},
                ),
              ),
              RemoteFocusFrame(
                focusNode: playPause,
                onActivate: () {},
                child: const SizedBox.square(dimension: 48),
              ),
            ],
          ),
        ),
      ),
    );
    progress.requestFocus();
    await tester.pump();

    await tester.sendKeyEvent(LogicalKeyboardKey.arrowDown);
    await tester.pump();
    expect(playPause.hasPrimaryFocus, isTrue);

    progress.requestFocus();
    await tester.pump();
    await tester.sendKeyEvent(LogicalKeyboardKey.arrowUp);
    await tester.pump();
    expect(settings.hasPrimaryFocus, isTrue);
  });

  testWidgets('focus frame preserves the progress bar width', (tester) async {
    const paintKey = ValueKey<String>('progress-paint');
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: SizedBox(
            width: 240,
            child: RemoteFocusFrame(
              child: const SizedBox(
                key: paintKey,
                height: 40,
                child: CustomPaint(),
              ),
            ),
          ),
        ),
      ),
    );

    expect(tester.getSize(find.byKey(paintKey)).width, 240);
  });

  testWidgets('focus frame follows traditional and touch highlight modes', (
    tester,
  ) async {
    final focusNode = FocusNode(debugLabel: 'programmatic-focus');
    addTearDown(focusNode.dispose);
    FocusManager.instance.highlightStrategy =
        FocusHighlightStrategy.alwaysTraditional;
    addTearDown(() {
      FocusManager.instance.highlightStrategy =
          FocusHighlightStrategy.automatic;
    });

    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: RemoteFocusFrame(
            focusNode: focusNode,
            child: const SizedBox.square(dimension: 48),
          ),
        ),
      ),
    );
    focusNode.requestFocus();
    await tester.pumpAndSettle();

    double borderAlpha() {
      final container = tester.widget<AnimatedContainer>(
        find.byType(AnimatedContainer),
      );
      final decoration = container.foregroundDecoration! as ShapeDecoration;
      return (decoration.shape as OutlinedBorder).side.color.a;
    }

    expect(focusNode.hasPrimaryFocus, isTrue);
    expect(borderAlpha(), greaterThan(0));

    FocusManager.instance.highlightStrategy =
        FocusHighlightStrategy.alwaysTouch;
    await tester.pumpAndSettle();

    expect(focusNode.hasPrimaryFocus, isTrue);
    expect(borderAlpha(), 0);
  });

  testWidgets('previous and next controls select opposite flip directions', (
    tester,
  ) async {
    PlaybackCommand? command;
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: PlayerView(
            state: const AppState(
              playback: PlaybackSnapshot(
                title: 'Track',
                durationMs: 180000,
                streamActive: true,
              ),
            ),
            onPlayback: (value) => command = value,
            onSeek: (_) {},
            onVolume: (_) {},
          ),
        ),
      ),
    );
    await tester.pump();

    FocusManager.instance.rootScope.descendants
        .singleWhere((node) => node.debugLabel == 'player-previous')
        .requestFocus();
    await tester.pump();
    await tester.sendKeyEvent(LogicalKeyboardKey.select);
    await tester.pump();
    expect(command, PlaybackCommand.previous);
    expect(
      tester.widget<ArtworkCard>(find.byType(ArtworkCard)).flipDirection,
      ArtworkFlipDirection.right,
    );

    FocusManager.instance.rootScope.descendants
        .singleWhere((node) => node.debugLabel == 'player-next')
        .requestFocus();
    await tester.pump();
    await tester.sendKeyEvent(LogicalKeyboardKey.select);
    await tester.pump();
    expect(command, PlaybackCommand.next);
    expect(
      tester.widget<ArtworkCard>(find.byType(ArtworkCard)).flipDirection,
      ArtworkFlipDirection.left,
    );
  });

  testWidgets('playback controls match Windows press motion', (tester) async {
    await tester.pumpWidget(
      MaterialApp(
        theme: ThemeData(splashFactory: NoSplash.splashFactory),
        home: Scaffold(
          body: PlayerView(
            state: const AppState(
              playback: PlaybackSnapshot(
                title: 'Track',
                durationMs: 180000,
                streamActive: true,
              ),
            ),
            onPlayback: (_) {},
            onSeek: (_) {},
            onVolume: (_) {},
          ),
        ),
      ),
    );
    await tester.pump();

    for (final key in const [
      'playback-previous-button',
      'playback-play-pause-button',
      'playback-next-button',
    ]) {
      final control = find.byKey(ValueKey(key));
      final scale = find.descendant(
        of: control,
        matching: find.byType(AnimatedScale),
      );
      final gesture = await tester.startGesture(tester.getCenter(control));
      await tester.pump();
      expect(tester.widget<AnimatedScale>(scale).scale, .97);
      expect(
        tester.widget<AnimatedScale>(scale).duration,
        const Duration(milliseconds: 120),
      );

      await gesture.up();
      await tester.pump();
      expect(tester.widget<AnimatedScale>(scale).scale, 1);
      expect(
        tester.widget<AnimatedScale>(scale).duration,
        const Duration(milliseconds: 160),
      );
    }
  });

  testWidgets('remote select applies the same playback press motion', (
    tester,
  ) async {
    var activations = 0;
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: PlayerView(
            state: const AppState(
              playback: PlaybackSnapshot(
                title: 'Track',
                durationMs: 180000,
                streamActive: true,
              ),
            ),
            onPlayback: (_) => activations++,
            onSeek: (_) {},
            onVolume: (_) {},
          ),
        ),
      ),
    );
    await tester.pump();

    final playPause = FocusManager.instance.rootScope.descendants.singleWhere(
      (node) => node.debugLabel == 'player-play-pause',
    );
    playPause.requestFocus();
    await tester.pump();
    final control = find.byKey(const ValueKey('playback-play-pause-button'));
    final scale = find.descendant(
      of: control,
      matching: find.byType(AnimatedScale),
    );

    await tester.sendKeyDownEvent(LogicalKeyboardKey.select);
    await tester.pump();
    expect(tester.widget<AnimatedScale>(scale).scale, .97);
    expect(
      tester.widget<AnimatedScale>(scale).duration,
      const Duration(milliseconds: 120),
    );

    await tester.sendKeyUpEvent(LogicalKeyboardKey.select);
    await tester.pump();
    expect(tester.widget<AnimatedScale>(scale).scale, 1);
    expect(
      tester.widget<AnimatedScale>(scale).duration,
      const Duration(milliseconds: 160),
    );
    expect(activations, 1);
  });

  testWidgets('disabling a focused control clears its remote press state', (
    tester,
  ) async {
    var enabled = true;
    late StateSetter update;
    final pressStates = <bool>[];
    await tester.pumpWidget(
      MaterialApp(
        home: StatefulBuilder(
          builder: (context, setState) {
            update = setState;
            return RemoteFocusFrame(
              autofocus: true,
              enabled: enabled,
              onActivate: () {},
              onPressChange: pressStates.add,
              child: const SizedBox.square(dimension: 48),
            );
          },
        ),
      ),
    );
    await tester.pump();

    await tester.sendKeyDownEvent(LogicalKeyboardKey.select);
    await tester.pump();
    expect(pressStates, [true]);

    update(() => enabled = false);
    await tester.pump();
    expect(pressStates, [true, false]);

    await tester.sendKeyUpEvent(LogicalKeyboardKey.select);
    await tester.pump();
    expect(pressStates, [true, false]);
  });

  testWidgets('playback press motion respects reduced motion', (tester) async {
    await tester.pumpWidget(
      const MaterialApp(
        home: MediaQuery(
          data: MediaQueryData(disableAnimations: true),
          child: Center(
            child: AnimatedPressScale(child: SizedBox.square(dimension: 48)),
          ),
        ),
      ),
    );

    final control = find.byType(AnimatedPressScale);
    final gesture = await tester.startGesture(tester.getCenter(control));
    await tester.pump();
    expect(tester.widget<AnimatedScale>(find.byType(AnimatedScale)).scale, 1);
    await gesture.up();
  });

  testWidgets('sender track direction controls the artwork flip', (
    tester,
  ) async {
    Widget player(
      String identity,
      TrackChangeDirection? trackChangeDirection,
    ) => MaterialApp(
      home: Scaffold(
        body: PlayerView(
          artworkIdentity: identity,
          state: AppState(
            playback: PlaybackSnapshot(
              title: identity,
              durationMs: 180000,
              streamActive: true,
              trackChangeDirection: trackChangeDirection,
            ),
          ),
          onPlayback: (_) {},
          onSeek: (_) {},
          onVolume: (_) {},
        ),
      ),
    );

    await tester.pumpWidget(player('track-a', null));
    await tester.pump();
    await tester.pumpWidget(player('track-b', TrackChangeDirection.previous));
    await tester.pump();
    expect(
      tester.widget<ArtworkCard>(find.byType(ArtworkCard)).flipDirection,
      ArtworkFlipDirection.right,
    );

    await tester.pumpWidget(player('track-c', TrackChangeDirection.next));
    await tester.pump();
    expect(
      tester.widget<ArtworkCard>(find.byType(ArtworkCard)).flipDirection,
      ArtworkFlipDirection.left,
    );
  });

  testWidgets(
    'track history infers direction when sender reports current only',
    (tester) async {
      Widget player(String identity) => MaterialApp(
        home: Scaffold(
          body: PlayerView(
            artworkIdentity: identity,
            state: AppState(
              playback: PlaybackSnapshot(
                title: identity,
                durationMs: 180000,
                streamActive: true,
              ),
            ),
            onPlayback: (_) {},
            onSeek: (_) {},
            onVolume: (_) {},
          ),
        ),
      );

      await tester.pumpWidget(player('track-a'));
      await tester.pump();
      await tester.pumpWidget(player('track-b'));
      await tester.pump();
      expect(
        tester.widget<ArtworkCard>(find.byType(ArtworkCard)).flipDirection,
        ArtworkFlipDirection.left,
      );

      await tester.pumpWidget(player('track-a'));
      await tester.pump();
      expect(
        tester.widget<ArtworkCard>(find.byType(ArtworkCard)).flipDirection,
        ArtworkFlipDirection.right,
      );
    },
  );

  testWidgets('play pause uses explicit left and right focus targets', (
    tester,
  ) async {
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: PlayerView(
            state: const AppState(
              playback: PlaybackSnapshot(
                title: 'Track',
                durationMs: 180000,
                streamActive: true,
              ),
            ),
            onPlayback: (_) {},
            onSeek: (_) {},
            onVolume: (_) {},
          ),
        ),
      ),
    );
    await tester.pump();

    final playPause = FocusManager.instance.rootScope.descendants.singleWhere(
      (node) => node.debugLabel == 'player-play-pause',
    );
    playPause.requestFocus();
    await tester.pump();
    await tester.sendKeyEvent(LogicalKeyboardKey.arrowLeft);
    await tester.pump();
    expect(FocusManager.instance.primaryFocus?.debugLabel, 'player-previous');

    playPause.requestFocus();
    await tester.pump();
    await tester.sendKeyEvent(LogicalKeyboardKey.arrowRight);
    await tester.pump();
    expect(FocusManager.instance.primaryFocus?.debugLabel, 'player-next');
  });
}

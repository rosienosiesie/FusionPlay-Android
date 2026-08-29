import 'package:flutter_test/flutter_test.dart';
import 'package:fusionplay_android_flutter/models/app_state.dart';

void main() {
  test('沿用 schema 6 设置时高级效果默认关闭', () {
    final state = AppState.fromMap({
      'initialized': true,
      'receiverName': '客厅 FusionPlay',
      'settings': {
        'schemaVersion': 6,
        'receiverName': '客厅 FusionPlay',
        'startupEnabled': false,
        'miPlayEnabled': true,
        'miPlayDeviceIdentity': 'tablet',
        'airPlayEnabled': false,
        'dlnaEnabled': true,
      },
    });

    expect(state.initialized, isTrue);
    expect(state.receiverName, '客厅 FusionPlay');
    expect(state.settings.schemaVersion, 6);
    expect(state.settings.autoWakeEnabled, isTrue);
    expect(state.settings.advancedEffectsEnabled, isFalse);
    expect(state.settings.miPlayDeviceIdentity, MiPlayDeviceIdentity.tablet);
    expect(state.settings.airPlayEnabled, isFalse);
  });

  test('解析 schema 7 高级效果设置', () {
    final settings = AppSettings.fromMap(const {
      'schemaVersion': 7,
      'advancedEffectsEnabled': true,
    });

    expect(settings.schemaVersion, 7);
    expect(settings.advancedEffectsEnabled, isTrue);
  });

  test('解析 schema 8 自动唤起设置', () {
    final settings = AppSettings.fromMap(const {
      'schemaVersion': 8,
      'autoWakeEnabled': false,
    });

    expect(settings.schemaVersion, 8);
    expect(settings.autoWakeEnabled, isFalse);
  });

  test('旧家庭屏标识按原兼容规则转换为音响', () {
    expect(
      MiPlayDeviceIdentity.parse('display_speaker'),
      MiPlayDeviceIdentity.speaker,
    );
  });

  test('同一曲目的不同封面来源共用一个动画身份', () {
    const first = PlaybackSnapshot(
      trackIdentity: 'track-42',
      coverArt: 'https://example.test/cover-small.jpg',
    );
    const second = PlaybackSnapshot(
      trackIdentity: 'track-42',
      coverArt: 'https://example.test/cover-large.jpg',
    );

    expect(first.artworkTransitionIdentity, second.artworkTransitionIdentity);
    expect(first.artworkTransitionIdentity, 'track:track-42');
  });

  test('解析发送端的上一曲和下一曲方向', () {
    final previous = PlaybackSnapshot.fromMap(const {
      'trackChangeDirection': 'previous',
    });
    final next = PlaybackSnapshot.fromMap(const {
      'trackChangeDirection': 'next',
    });

    expect(previous.trackChangeDirection, TrackChangeDirection.previous);
    expect(next.trackChangeDirection, TrackChangeDirection.next);
  });

  test('值相同的原生状态可去重且命令集合不依赖顺序', () {
    final first = AppState.fromMap({
      'initialized': true,
      'receiverName': '客厅 FusionPlay',
      'playback': {'title': '歌曲', 'positionMs': 5200},
      'remoteControl': {
        'available': true,
        'commands': ['play', 'pause', 'seek'],
      },
    });
    final second = AppState.fromMap({
      'initialized': true,
      'receiverName': '客厅 FusionPlay',
      'playback': {'title': '歌曲', 'positionMs': 5200},
      'remoteControl': {
        'available': true,
        'commands': ['seek', 'pause', 'play'],
      },
    });

    expect(first, second);
    expect(first.hashCode, second.hashCode);
    expect(
      first,
      isNot(
        AppState.fromMap({
          'initialized': true,
          'receiverName': '客厅 FusionPlay',
          'playback': {'title': '歌曲', 'positionMs': 5201},
          'remoteControl': {
            'available': true,
            'commands': ['play', 'pause', 'seek'],
          },
        }),
      ),
    );
  });
}

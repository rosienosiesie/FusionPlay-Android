import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:fusionplay_android_flutter/ui/vector_icon.dart';

void main() {
  test('界面 SVG 与 MiSans 字体均被声明并保留', () {
    final pubspec = File('pubspec.yaml').readAsStringSync();
    for (final asset in FusionIcons.all) {
      expect(File(asset).existsSync(), isTrue, reason: '缺少 $asset');
      expect(pubspec, contains('- $asset'));
    }

    const font = 'assets/fonts/MiSansVF.ttf';
    expect(File(font).existsSync(), isTrue);
    expect(File(font).lengthSync(), greaterThan(0));
    expect(pubspec, contains('- asset: $font'));
  });

  test('传统 launcher 图标位于 Android 识别的密度目录', () {
    for (final density in const [
      'mdpi',
      'hdpi',
      'xhdpi',
      'xxhdpi',
      'xxxhdpi',
    ]) {
      final directory = 'android/app/src/main/res/mipmap-$density';
      expect(File('$directory/ic_launcher_v3.png').existsSync(), isTrue);
      expect(File('$directory/ic_launcher_round_v3.png').existsSync(), isTrue);
    }
  });
}

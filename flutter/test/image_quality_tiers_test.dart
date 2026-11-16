import 'package:flutter_hbb/common/image_quality_tiers.dart';
import 'package:flutter_hbb/consts.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  final game = imageQualityTierById('tier-game')!;
  final ultra = imageQualityTierById('tier-ultra')!;
  final high = imageQualityTierById('tier-high')!;

  group('imageQualityTierOf', () {
    test('a custom quality at exactly a tier\'s values is that tier', () {
      for (final tier in kImageQualityTiers) {
        expect(
            imageQualityTierOf(
                kRemoteImageQualityCustom, tier.quality, tier.fps),
            same(tier));
      }
    });

    test('any other custom quality is just custom', () {
      expect(imageQualityTierOf(kRemoteImageQualityCustom, 300, 30), isNull);
      expect(imageQualityTierOf(kRemoteImageQualityCustom, 290, 60), isNull);
      expect(imageQualityTierOf(kRemoteImageQualityCustom, null, 60), isNull);
      expect(imageQualityTierOf(kRemoteImageQualityCustom, 300, null), isNull);
    });

    test('a preset is not a tier, whatever the custom values left behind', () {
      for (final preset in [
        kRemoteImageQualityBest,
        kRemoteImageQualityBalanced,
        kRemoteImageQualityLow,
        ''
      ]) {
        expect(imageQualityTierOf(preset, 300, 60), isNull);
      }
    });

    test('no two tiers stand for the same values, nor share an id', () {
      final values = kImageQualityTiers.map((t) => '${t.quality}@${t.fps}');
      expect(values.toSet().length, kImageQualityTiers.length);
      final ids = kImageQualityTiers.map((t) => t.id).toSet();
      expect(ids.length, kImageQualityTiers.length);
      // An id is a radio value next to the presets and "custom": it must not be one of them.
      expect(
          ids.intersection({
            kRemoteImageQualityBest,
            kRemoteImageQualityBalanced,
            kRemoteImageQualityLow,
            kRemoteImageQualityCustom
          }),
          isEmpty);
    });
  });

  group('imageQualityTierBlocked', () {
    test('a direct connection may have any tier, whatever the peer', () {
      for (final tier in kImageQualityTiers) {
        expect(
            imageQualityTierBlocked(tier, direct: true, peerMeasures: false),
            isNull);
        expect(imageQualityTierBlocked(tier, direct: true, peerMeasures: true),
            isNull);
      }
    });

    test('through the relay the game tier is not to be had', () {
      expect(imageQualityTierBlocked(game, direct: false, peerMeasures: true),
          kImageQualityNeedsDirect);
      expect(imageQualityTierBlocked(game, direct: false, peerMeasures: false),
          kImageQualityNeedsDirect);
    });

    test('through the relay the others need a peer that measures', () {
      for (final tier in [ultra, high]) {
        expect(imageQualityTierBlocked(tier, direct: false, peerMeasures: true),
            isNull);
        expect(
            imageQualityTierBlocked(tier, direct: false, peerMeasures: false),
            kImageQualityNeedsNewPeer);
      }
    });
  });

  test('a number option reads as an integer however it was stored', () {
    expect(imageQualityOptionValue('300'), 300);
    expect(imageQualityOptionValue('300.0'), 300);
    expect(imageQualityOptionValue('60'), 60);
    expect(imageQualityOptionValue(''), isNull);
    expect(imageQualityOptionValue(null), isNull);
  });
}

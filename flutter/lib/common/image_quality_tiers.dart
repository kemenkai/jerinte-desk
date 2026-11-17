import 'package:flutter_hbb/consts.dart';

/// An image quality with a name of its own above the presets: the custom quality at a fixed
/// percentage and frame rate. Only the menus know of it. What is stored and what is sent is the
/// custom quality, so a peer or a configuration from before the names is none the wiser.
class ImageQualityTier {
  /// The value of its radio entry. Never stored, never sent.
  final String id;

  /// Translation keys: its name, and what it takes at 1080p.
  final String label;
  final String tip;

  /// Translation key of what it takes besides its bitrate, written after that, for a session
  /// whose picture's size is known (`imageQualityRateAt`); `{}` is the bandwidth, in Mbps.
  final String need;

  /// The custom image quality, in percent, and the frame rate it stands for.
  final int quality;
  final int fps;

  /// Only for a direct connection: through the relay the controlled side keeps the stream under
  /// the relay's limit, far below this.
  final bool directOnly;

  const ImageQualityTier({
    required this.id,
    required this.label,
    required this.tip,
    required this.need,
    required this.quality,
    required this.fps,
    this.directOnly = false,
  });
}

/// Best first, as the menus list them.
const kImageQualityTiers = <ImageQualityTier>[
  ImageQualityTier(
      id: 'tier-game',
      label: 'quality-game',
      tip: 'quality-game-tip',
      need: 'quality-game-need {}',
      quality: 500,
      fps: 60,
      directOnly: true),
  ImageQualityTier(
      id: 'tier-ultra',
      label: 'quality-ultra',
      tip: 'quality-ultra-tip',
      need: 'quality-ultra-need {}',
      quality: 300,
      fps: 60),
  ImageQualityTier(
      id: 'tier-high',
      label: 'quality-high',
      tip: 'quality-high-tip',
      need: 'quality-high-need {}',
      quality: 150,
      fps: 60),
];

/// The bitrate ratio of a tier: the custom quality's percentage counts double, as the controlled
/// side takes it (`user_image_quality` in src/server/video_qos.rs).
extension ImageQualityTierRatio on ImageQualityTier {
  double get ratio => quality * 2 / 100;
}

/// The ratios of the presets (`BR_BEST`, `BR_BALANCED`, `BR_SPEED` in
/// libs/scrap/src/common/codec.rs), and what they take besides their bitrate.
const kImageQualityRatioBest = 1.5;
const kImageQualityRatioBalanced = 0.67;
const kImageQualityRatioLow = 0.5;
const kImageQualityPresetNeed = 'quality-30-fps';

/// Translation key of a bitrate, in Mbps.
const kImageQualityAbout = 'quality-about {}';

/// The bitrate at ratio 1.0 of a picture of these sizes, in kbps: (width, height, kbps), as the
/// controlled side reckons it (`base_bitrate` in libs/scrap/src/common/codec.rs, which a test in
/// src/lang.rs holds this table to).
const kImageQualityBaseBitrates = <List<int>>[
  [640, 480, 400],
  [800, 600, 500],
  [1024, 768, 800],
  [1280, 720, 1000],
  [1366, 768, 1100],
  [1440, 900, 1300],
  [1600, 900, 1500],
  [1920, 1080, 2073],
  [2048, 1080, 2200],
  [2560, 1440, 3000],
  [3440, 1440, 4000],
  [3840, 2160, 5000],
  [7680, 4320, 12000],
];

/// The bitrate at ratio 1.0 of a picture of [width] x [height], in kbps: that of the size above
/// nearest in pixels, for as many pixels as it has.
int imageQualityBaseKbps(int width, int height) {
  final pixels = width * height;
  var nearest = kImageQualityBaseBitrates[7];
  int? distance;
  for (final size in kImageQualityBaseBitrates) {
    final d = (size[0] * size[1] - pixels).abs();
    if (distance == null || d < distance) {
      nearest = size;
      distance = d;
    }
  }
  return (nearest[2] * pixels / (nearest[0] * nearest[1])).round();
}

/// What a quality at [ratio] comes to for a picture of [width] x [height]: its bitrate, as shown
/// (Mbps, to a tenth below ten), and the bandwidth that takes (Mbps: half again the bitrate, to
/// the nearest five, and five at least).
({String mbps, int bandwidth}) imageQualityRateAt(
    int width, int height, double ratio) {
  final mbps = imageQualityBaseKbps(width, height) * ratio / 1000;
  final shown = mbps < 10
      ? mbps.toStringAsFixed(1).replaceFirst(RegExp(r'\.0$'), '')
      : mbps.round().toString();
  final bandwidth = (mbps * 1.5 / 5).round() * 5;
  return (mbps: shown, bandwidth: bandwidth < 5 ? 5 : bandwidth);
}

/// Why a tier cannot be chosen, as translation keys.
const kImageQualityNeedsDirect = 'quality-needs-direct-tip';
const kImageQualityNeedsNewPeer = 'quality-needs-new-peer-tip';

/// The tier with this radio value, if it is one.
ImageQualityTier? imageQualityTierById(String? id) {
  for (final tier in kImageQualityTiers) {
    if (tier.id == id) return tier;
  }
  return null;
}

/// The tier a quality setting is: the custom quality at exactly a tier's values.
ImageQualityTier? imageQualityTierOf(
    String imageQuality, int? customQuality, int? customFps) {
  if (imageQuality != kRemoteImageQualityCustom) return null;
  for (final tier in kImageQualityTiers) {
    if (tier.quality == customQuality && tier.fps == customFps) return tier;
  }
  return null;
}

/// Why [tier] cannot be chosen for a session, as a translation key; null when it can.
///
/// [direct]: the connection does not go through the relay. [peerMeasures]: the controlled side
/// sizes its bitrate by what the path carries (`Features.measured_qos`); through the relay, one
/// that does not is asked for no more than the best preset at thirty frames, whatever is chosen.
String? imageQualityTierBlocked(ImageQualityTier tier,
    {required bool direct, required bool peerMeasures}) {
  if (direct) return null;
  if (tier.directOnly) return kImageQualityNeedsDirect;
  if (!peerMeasures) return kImageQualityNeedsNewPeer;
  return null;
}

/// A number option as the settings store it ("300", "300.0"), as an integer.
int? imageQualityOptionValue(String? option) {
  if (option == null) return null;
  return double.tryParse(option)?.round();
}

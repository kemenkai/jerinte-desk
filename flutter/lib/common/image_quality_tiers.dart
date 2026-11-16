import 'package:flutter_hbb/consts.dart';

/// An image quality with a name of its own above the presets: the custom quality at a fixed
/// percentage and frame rate. Only the menus know of it. What is stored and what is sent is the
/// custom quality, so a peer or a configuration from before the names is none the wiser.
class ImageQualityTier {
  /// The value of its radio entry. Never stored, never sent.
  final String id;

  /// Translation keys: its name, and what it takes.
  final String label;
  final String tip;

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
      quality: 500,
      fps: 60,
      directOnly: true),
  ImageQualityTier(
      id: 'tier-ultra',
      label: 'quality-ultra',
      tip: 'quality-ultra-tip',
      quality: 300,
      fps: 60),
  ImageQualityTier(
      id: 'tier-high',
      label: 'quality-high',
      tip: 'quality-high-tip',
      quality: 150,
      fps: 60),
];

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

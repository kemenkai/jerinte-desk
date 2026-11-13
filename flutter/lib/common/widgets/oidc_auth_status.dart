import 'package:flutter/material.dart';
import 'package:qr_flutter/qr_flutter.dart';

const _statusFontSize = 12.0;
const _statusSpacing = 4.0;
const _messageActionSpacing = 8.0;
const _desktopActionSize = 28.0;
const _qrSize = 180.0;
const _touchPlatforms = <TargetPlatform>{
  TargetPlatform.android,
  TargetPlatform.iOS,
  TargetPlatform.fuchsia,
};

class OidcAuthStatus extends StatelessWidget {
  final String message;
  final String browserFallbackPrompt;
  final String authUrl;
  final String copyLabel;
  final VoidCallback? onCopy;

  /// Show the URL as a QR code at once, for a machine that cannot open the
  /// page itself. Otherwise the code comes with the full URL, once asked for.
  final bool showQr;

  const OidcAuthStatus({
    super.key,
    required this.message,
    required this.browserFallbackPrompt,
    required this.authUrl,
    required this.copyLabel,
    this.onCopy,
    this.showQr = false,
  });

  @override
  Widget build(BuildContext context) {
    final messageStyle =
        DefaultTextStyle.of(context).style.copyWith(fontSize: _statusFontSize);
    return Column(
      mainAxisSize: MainAxisSize.min,
      children: [
        SelectableText(message, style: messageStyle),
        if (authUrl.isNotEmpty)
          Padding(
            padding: const EdgeInsets.only(top: _messageActionSpacing),
            child: _OidcAuthFallback(
              browserFallbackPrompt: browserFallbackPrompt,
              authUrl: authUrl,
              copyLabel: copyLabel,
              onCopy: onCopy,
              showQr: showQr,
            ),
          ),
      ],
    );
  }
}

class _OidcAuthFallback extends StatefulWidget {
  final String browserFallbackPrompt;
  final String authUrl;
  final String copyLabel;
  final VoidCallback? onCopy;
  final bool showQr;

  const _OidcAuthFallback({
    required this.browserFallbackPrompt,
    required this.authUrl,
    required this.copyLabel,
    required this.onCopy,
    required this.showQr,
  });

  @override
  State<_OidcAuthFallback> createState() => _OidcAuthFallbackState();
}

class _OidcAuthFallbackState extends State<_OidcAuthFallback> {
  bool _expanded = false;

  @override
  void didUpdateWidget(covariant _OidcAuthFallback oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (oldWidget.authUrl != widget.authUrl) {
      _expanded = false;
    }
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final helperStyle = DefaultTextStyle.of(context).style.copyWith(
          fontSize: _statusFontSize,
          color: theme.colorScheme.onSurfaceVariant,
        );
    final linkColor = theme.brightness == Brightness.dark
        ? Colors.blue.shade300
        : Colors.blue.shade800;
    final isTouchPlatform = _touchPlatforms.contains(theme.platform);
    final actionSize =
        isTouchPlatform ? kMinInteractiveDimension : _desktopActionSize;
    final urlStyle =
        DefaultTextStyle.of(context).style.copyWith(fontSize: _statusFontSize);
    return Column(
      mainAxisSize: MainAxisSize.min,
      children: [
        Text(
          widget.browserFallbackPrompt,
          style: helperStyle,
          textAlign: TextAlign.center,
        ),
        // The same URL for a phone to open: signing in there signs this
        // client in, which keeps asking the server about its request.
        if (widget.showQr || _expanded)
          Padding(
            padding: const EdgeInsets.only(top: _messageActionSpacing),
            child: QrImageView(
              data: widget.authUrl,
              size: _qrSize,
              backgroundColor: Colors.white,
              version: QrVersions.auto,
              gapless: false,
            ),
          ),
        Padding(
          padding: const EdgeInsets.only(top: _statusSpacing),
          child: _buildUrl(urlStyle, linkColor, actionSize),
        ),
      ],
    );
  }

  void _copyAndExpand() {
    setState(() => _expanded = true);
    widget.onCopy?.call();
  }

  Widget _buildUrl(TextStyle urlStyle, Color linkColor, double actionSize) {
    final collapsedUrl = SizedBox(
      width: double.infinity,
      child: TextButton(
        style: TextButton.styleFrom(
          foregroundColor: linkColor,
          minimumSize: Size(0, actionSize),
          padding: EdgeInsets.zero,
          tapTargetSize: MaterialTapTargetSize.shrinkWrap,
          visualDensity: VisualDensity.standard,
        ),
        onPressed: _copyAndExpand,
        child: Text(
          widget.authUrl,
          maxLines: 1,
          overflow: TextOverflow.ellipsis,
          softWrap: false,
          style: urlStyle.copyWith(
            color: linkColor,
            decoration: TextDecoration.underline,
          ),
        ),
      ),
    );
    final collapsedChild = widget.onCopy == null
        ? collapsedUrl
        : Tooltip(message: widget.copyLabel, child: collapsedUrl);
    return Container(
      width: double.infinity,
      constraints: BoxConstraints(minHeight: actionSize),
      alignment: Alignment.centerLeft,
      padding: const EdgeInsets.symmetric(horizontal: _messageActionSpacing),
      decoration: BoxDecoration(
        border: Border.all(color: Theme.of(context).dividerColor),
        borderRadius: BorderRadius.circular(_statusSpacing),
      ),
      child: _expanded
          ? SelectableText(widget.authUrl, style: urlStyle)
          : collapsedChild,
    );
  }
}

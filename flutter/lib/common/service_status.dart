// What the service is connected to on our servers, as the status bar shows it: the `services`
// snapshot of mainGetConnectStatus, made by src/service_status.rs. Pure, the caller passing the
// translation, so that it can be tested.

typedef Translate = String Function(String);

/// The status word the snapshot says, 'ready', 'connecting' or 'notReady'; null to go by
/// `status_num` as upstream does: no snapshot, from a service before it, or the service stopped.
String? serviceWord(Map<String, dynamic>? s) {
  switch (s?['state']) {
    case 'registered':
      return 'ready';
    case 'picking':
    case 'connecting':
      return 'connecting';
    case 'unanswered':
    case 'waiting':
      return 'notReady';
  }
  return null;
}

/// What follows the status word, only what is true now; empty for nothing to say.
String serviceSummary(Map<String, dynamic>? s, Translate tr) {
  final id = _map(s?['id']);
  switch (s?['state']) {
    case 'picking':
      return tr('Picking a server entry');
    case 'connecting':
      return _target(id, tr, proxy: true);
    case 'registered':
      final api = _map(s?['api']);
      return [
        _target(id, tr, proxy: true),
        if (api != null)
          _words(['API ${api['port']}', if (api['ok'] != true) tr('Failed')]),
      ].where((x) => x.isNotEmpty).join(' · ');
    case 'unanswered':
      return _words([_target(id, tr), tr('no answer')]);
    case 'waiting':
      return _words([_target(id, tr), _retry(s!, tr)]);
  }
  return '';
}

/// Why and how long the wait is: a connection that dropped, or a server that could not be reached.
String _retry(Map<String, dynamic> s, Translate tr) => s['dropped'] == true
    ? tr('disconnected, retry in {${s['retry_in_s']}} s')
    : tr('unreachable, retry in {${s['retry_in_s']}} s');

/// The tooltip: all that is known, addresses as host and port; empty for nothing to say.
String serviceDetails(Map<String, dynamic>? s, Translate tr) {
  final state = s?['state'];
  if (s == null || state == 'stopped') return '';
  final id = _map(s['id']);
  final entry = id?['entry'];
  final lines = <String>[];
  if (entry is String) {
    final label = tr(entry);
    lines.add(
        tr('Server entry: {${label == entry ? entry : '$label ($entry)'}}'));
  }
  if (id != null) {
    final parts = [tr('ID server: {${_address(id)}}')];
    switch (state) {
      case 'registered':
        parts.add(tr('registered for {${_duration(id['for_s'], tr)}}'));
        final rtt = id['rtt_us'];
        if (rtt is int && rtt > 0) {
          final ms = '${(rtt / 1000).toStringAsFixed(1)} ms';
          parts.add(id['rtt_kind'] == 'average'
              ? tr('average latency {$ms}')
              : tr('latency at registration {$ms}'));
        }
        break;
      case 'connecting':
        parts.add(tr('Connecting to the server'));
        break;
      case 'unanswered':
        parts.add(tr('no answer'));
        break;
      case 'waiting':
        parts.add(_retry(s, tr));
        break;
    }
    lines.add(parts.join(' · '));
  }
  final api = _map(s['api']);
  if (api != null) {
    final ago = _duration(api['ago_s'], tr);
    lines.add([
      tr('API: {${api['host']}:${api['port']}}'),
      api['ok'] == true
          ? tr('heartbeat succeeded {$ago} ago')
          : tr('heartbeat failed {$ago} ago'),
      if (api['error'] is String) api['error'] as String,
      // The API is one address for every entry.
      if (entry is String) tr('not switched with the entry'),
    ].join(' · '));
  }
  if (id?['proxy'] is String) {
    lines.add(tr('Proxy: {${id!['proxy']}}'));
  }
  final error = _map(s['error']);
  if (error != null) {
    final target = _map(error['target']);
    lines.add([
      tr('Last failure: {${target == null ? '-' : _address(target)}}'),
      '${error['text']}',
      tr('{${_duration(error['ago_s'], tr)}} ago'),
    ].join(' · '));
  }
  return lines.join('\n');
}

Map<String, dynamic>? _map(dynamic v) =>
    v is Map ? v.cast<String, dynamic>() : null;

String _words(List<String> parts) => parts.where((x) => x.isNotEmpty).join(' ');

/// The way and port, as in `内网 TCP 21116`.
String _target(Map<String, dynamic>? id, Translate tr, {bool proxy = false}) {
  if (id == null) return '';
  return _words([
    if (id['entry'] is String) tr(id['entry'] as String),
    '${id['via']}'.toUpperCase(),
    '${id['port']}',
    if (proxy && id['proxy'] is String) tr('via proxy'),
  ]);
}

/// The way and address, as in `TCP 192.168.1.10:21116`.
String _address(Map<String, dynamic> id) =>
    '${'${id['via']}'.toUpperCase()} ${id['host']}:${id['port']}';

String _duration(dynamic seconds, Translate tr) {
  final s = seconds is int ? seconds : 0;
  if (s < 60) return tr('{$s} s');
  if (s < 3600) return tr('{${s ~/ 60}} min');
  return tr('{${s ~/ 3600}} h');
}

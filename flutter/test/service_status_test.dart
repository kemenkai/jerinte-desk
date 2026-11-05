import 'package:flutter_test/flutter_test.dart';
import 'package:flutter_hbb/common/service_status.dart';

// The Chinese of src/lang/cn.rs, looked up as src/lang.rs does: the first `{value}` becomes `{}`
// in the key and the value goes back in its place; a key without a translation is itself.
const cn = {
  'Picking a server entry': '正在选入口',
  'unreachable, retry in {} s': '连不上，{} 秒后重试',
  'disconnected, retry in {} s': '断开了，{} 秒后重试',
  'no answer': '无应答',
  'via proxy': '经代理',
  'Failed': '失败',
  'lan': '内网',
  'wan': '外网',
  'Server entry: {}': '入口：{}',
  'ID server: {}': 'ID 服务器：{}',
  'API: {}': 'API：{}',
  'Proxy: {}': '代理：{}',
  'Last failure: {}': '上次失败：{}',
  'registered for {}': '已注册 {}',
  'latency at registration {}': '注册时延迟 {}',
  'average latency {}': '延迟（平均）{}',
  'heartbeat succeeded {} ago': '{}前心跳成功',
  'heartbeat failed {} ago': '{}前心跳失败',
  'not switched with the entry': '不随入口切换',
  'Connecting to the server': '正在接入',
  '{} ago': '{}前',
  '{} s': '{} 秒',
  '{} min': '{} 分钟',
  '{} h': '{} 小时',
};

String tr(String name) {
  final m = RegExp(r'\{(.*?)\}').firstMatch(name);
  final key = m == null ? name : name.replaceFirst(m.group(0)!, '{}');
  final s = cn[key] ?? key;
  return m == null ? s : s.replaceAll('{}', m.group(1)!);
}

Map<String, dynamic> lan({String state = 'registered', Map? api}) => {
      'state': state,
      'id': {
        'via': 'tcp',
        'host': '192.168.1.10',
        'port': 21116,
        'proxy': null,
        'entry': 'lan',
        if (state == 'registered') ...{
          'for_s': 194,
          'rtt_us': 812,
          'rtt_kind': 'at_register',
        },
      },
      if (api != null) 'api': api,
    };

const api = {
  'host': 'app.example.com',
  'port': 4443,
  'ok': true,
  'ago_s': 12,
  'error': null,
};

void main() {
  test('the words go by the snapshot, upstream\'s without one', () {
    expect(serviceWord(null), null);
    expect(serviceWord({'state': 'stopped'}), null);
    expect(serviceWord(lan()), 'ready');
    expect(serviceWord({'state': 'picking'}), 'connecting');
    expect(serviceWord(lan(state: 'connecting')), 'connecting');
    expect(serviceWord(lan(state: 'waiting')), 'notReady');
    expect(serviceWord(lan(state: 'unanswered')), 'notReady');
  });

  test('the summary says only what is true now', () {
    expect(serviceSummary(lan(api: api), tr), '内网 TCP 21116 · API 4443');
    expect(serviceSummary(lan(api: {...api, 'ok': false}), tr),
        '内网 TCP 21116 · API 4443 失败');
    expect(serviceSummary(lan(), tr), '内网 TCP 21116');
    final wan = {
      'state': 'registered',
      'id': {
        'via': 'wss',
        'host': 'app.example.com',
        'port': 4443,
        'proxy': '10.0.0.1:1080',
        'entry': 'wan'
      },
      'api': api,
    };
    expect(serviceSummary(wan, tr), '外网 WSS 4443 经代理 · API 4443');
    expect(serviceSummary({'state': 'picking'}, tr), '正在选入口');
    expect(
        serviceSummary(lan(state: 'connecting', api: api), tr), '内网 TCP 21116');
    expect(serviceSummary({...lan(state: 'waiting'), 'retry_in_s': 23}, tr),
        '内网 TCP 21116 连不上，23 秒后重试');
    expect(
        serviceSummary(
            {...lan(state: 'waiting'), 'retry_in_s': 9, 'dropped': true}, tr),
        '内网 TCP 21116 断开了，9 秒后重试');
    final udp = {
      'state': 'unanswered',
      'id': {
        'via': 'udp',
        'host': 'rs.example.com',
        'port': 21116,
        'proxy': null,
        'entry': null
      },
    };
    expect(serviceSummary(udp, tr), 'UDP 21116 无应答');
    expect(serviceSummary(null, tr), '');
    expect(serviceSummary({'state': 'stopped'}, tr), '');
  });

  test('the details tell everything, as host and port', () {
    expect(
        serviceDetails(lan(api: api), tr),
        '入口：内网 (lan)\n'
        'ID 服务器：TCP 192.168.1.10:21116 · 已注册 3 分钟 · 注册时延迟 0.8 ms\n'
        'API：app.example.com:4443 · 12 秒前心跳成功 · 不随入口切换');
    final failed = {
      ...lan(state: 'waiting'),
      'retry_in_s': 23,
      'error': {
        'text': 'Connection refused (os error 61)',
        'ago_s': 7,
        'target': {'via': 'tcp', 'host': '192.168.1.10', 'port': 21116},
      },
    };
    expect(
        serviceDetails(failed, tr),
        '入口：内网 (lan)\n'
        'ID 服务器：TCP 192.168.1.10:21116 · 连不上，23 秒后重试\n'
        '上次失败：TCP 192.168.1.10:21116 · Connection refused (os error 61) · 7 秒前');
    final noEntry = {
      'state': 'registered',
      'id': {
        'via': 'udp',
        'host': 'rs.example.com',
        'port': 21116,
        'proxy': null,
        'entry': null,
        'for_s': 7200,
        'rtt_us': 31500,
        'rtt_kind': 'average',
      },
      'api': {
        ...api,
        'ok': false,
        'ago_s': 75,
        'error': 'the answer is no JSON'
      },
    };
    expect(
        serviceDetails(noEntry, tr),
        'ID 服务器：UDP rs.example.com:21116 · 已注册 2 小时 · 延迟（平均）31.5 ms\n'
        'API：app.example.com:4443 · 1 分钟前心跳失败 · the answer is no JSON');
    expect(serviceDetails({'state': 'picking'}, tr), '');
    expect(serviceDetails({'state': 'stopped'}, tr), '');
    expect(serviceDetails(null, tr), '');
  });

  test('an entry without a translation shows its name', () {
    final office = {
      'state': 'connecting',
      'id': {
        'via': 'tcp',
        'host': '10.1.2.3',
        'port': 21116,
        'proxy': null,
        'entry': 'office'
      },
    };
    expect(serviceSummary(office, tr), 'office TCP 21116');
    expect(serviceDetails(office, tr).split('\n').first, '入口：office');
  });
}

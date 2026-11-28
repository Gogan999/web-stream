# Drives the display helper against KWin's virtual outputs (one mode each) through kscreen-doctor.
import json, select, struct, subprocess, sys
helper = sys.argv[1]
def frame(p):
    b = json.dumps(p).encode(); return b'Q' + struct.pack('<I', len(b)) + b
def reply(p):
    assert select.select([p.stdout], [], [], 30)[0], 'no reply'
    h = p.stdout.read(5); assert h[:1] == b'A', h
    return json.loads(p.stdout.read(struct.unpack('<I', h[1:])[0]))
p = subprocess.Popen([helper, 'display-helper'], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
def send(x):
    p.stdin.write(frame(x)); p.stdin.flush(); return reply(p)
r = send({'set': {'connector': 'Virtual-0', 'edid': None, 'width': 1920, 'height': 1080}})
assert r == {'set': {'width': 1920, 'height': 1080, 'changed': False, 'remaining': 0}}, r
r = send({'set': {'connector': 'Virtual-1', 'edid': None, 'width': 16, 'height': 16}})
assert 'no resolution that fits' in r.get('failed', ''), r
r = send({'set': {'connector': 'HDMI-A-9', 'edid': None, 'width': 1280, 'height': 720}})
assert 'Virtual-0, Virtual-1' in r.get('failed', ''), r
r = send({'restore': {'connector': 'Virtual-1'}})
assert r == {'restored': {'remaining': 0}}, r
p.stdin.close(); assert p.wait(timeout=20) == 0
out = subprocess.run(['kscreen-doctor', 'output.Virtual-1.position.2000,0', 'output.Virtual-0.mode.0'],
                     capture_output=True, text=True)
assert 'QPoint(2000,0)' in out.stdout + out.stderr, out
print('kde ok')

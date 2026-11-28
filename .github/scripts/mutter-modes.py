# Drives the display helper against Mutter with two dummy monitors: change, neighbour move, restore.
import json, re, select, signal, struct, subprocess, sys, time
helper = sys.argv[1]
first, second = 'LVDS1', 'LVDS2'
def frame(p):
    b = json.dumps(p).encode(); return b'Q' + struct.pack('<I', len(b)) + b
def reply(p):
    assert select.select([p.stdout], [], [], 20)[0], 'no reply'
    h = p.stdout.read(5); assert h[:1] == b'A', h
    return json.loads(p.stdout.read(struct.unpack('<I', h[1:])[0]))
def outputs():
    s = subprocess.run(['gdbus', 'call', '--session', '--dest', 'org.gnome.Mutter.DisplayConfig',
        '--object-path', '/org/gnome/Mutter/DisplayConfig', '--method',
        'org.gnome.Mutter.DisplayConfig.GetCurrentState'], capture_output=True, text=True, check=True).stdout
    monitors, logical = s.split("], [(", 1)[0], s.split("], [(", 1)[1]
    cur = {}
    for m in re.finditer(r"\(\('(\w+)',.*?\), \[(.*?)\], \{'is-builtin'", s):
        name, modes = m.group(1), m.group(2)
        for mm in re.finditer(r"\('[^']+', (\d+), (\d+), [\d.]+, [\d.]+, \[[^\]]*\], (\{[^}]*\}|@a\{sv\} \{\})\)", modes):
            if 'is-current' in mm.group(3):
                cur[name] = (int(mm.group(1)), int(mm.group(2)))
    out = {}
    for m in re.finditer(r"\((-?\d+), (-?\d+), [\d.]+, (?:uint32 )?\d+, (?:true|false), \[\('(\w+)'", s):
        x, y, name = int(m.group(1)), int(m.group(2)), m.group(3)
        out[name] = cur[name] + (x, y)
    return out
def start():
    return subprocess.Popen([helper, 'display-helper'], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
def send(p, x):
    p.stdin.write(frame(x)); p.stdin.flush(); return reply(p)
def change(p, w, h):
    r = send(p, {'set': {'connector': first, 'edid': None, 'width': w, 'height': h}})
    assert 'set' in r, r
    return r['set']
before = outputs(); print('before', before)
fw, fh, fx, fy = before[first]
p = start()
got = change(p, 1024, 768); assert got == {'width': 1024, 'height': 768, 'changed': True, 'remaining': 1}, got
time.sleep(0.5); now = outputs(); print('changed', now)
assert now[first][:2] == (1024, 768), now
assert now[second][2] == fx + 1024, 'neighbour moved with the edge'
again = change(p, 1024, 768); assert again['changed'] is False and again['remaining'] == 1, again
r = send(p, {'restore': {'connector': first}}); assert r == {'restored': {'remaining': 0}}, r
time.sleep(0.5); assert outputs() == before, outputs()
change(p, 1280, 720); time.sleep(0.5); assert outputs()[first][:2] == (1280, 720), outputs()
p.stdin.close(); assert p.wait(timeout=20) == 0
time.sleep(0.5); assert outputs() == before, outputs()
p = start(); change(p, 1024, 768); time.sleep(0.5); assert outputs()[first][:2] == (1024, 768)
p.send_signal(signal.SIGTERM); assert p.wait(timeout=20) == 0
time.sleep(0.5); assert outputs() == before, outputs()
p = start(); r = send(p, {'set': {'connector': first, 'edid': None, 'width': 16, 'height': 16}}); assert 'failed' in r, r
p.stdin.close(); assert p.wait(timeout=20) == 0; assert outputs() == before
print('mutter ok')

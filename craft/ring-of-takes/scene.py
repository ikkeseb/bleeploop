"""Build and render the physical four-bar pressing. Render indices are zero-based."""
import argparse
import json
import math
import os
from pathlib import Path
import sys

import bpy
from mathutils import Vector

HERE = Path(__file__).resolve().parent
OUT = Path(os.getenv("PROMO_WORK", HERE / "output"))
MAP = json.loads((OUT / "map.json").read_text())
TAU = 2 * math.pi
LOOP_FRAMES = MAP["samples_per_loop"] * MAP["fps"] // MAP["sr"]
FRAMES_PER_BAR = LOOP_FRAMES // 4
parser = argparse.ArgumentParser()
parser.add_argument("--format", choices=["wide", "tall"], default="wide")
parser.add_argument("--frames", default="302", help="comma-separated indices or inclusive a-b")
parser.add_argument("--scale", type=int, default=100)
parser.add_argument("--samples", type=int, default=12)
parser.add_argument("--dir", default="draft")
parser.add_argument("--hero", action="store_true")
parser.add_argument("--audit", action="store_true")
parser.add_argument("--topology-only", action="store_true")
parser.add_argument("--project", action="store_true", help="Export the true per-frame surface projection for compositing")
args = parser.parse_args(sys.argv[sys.argv.index("--")+1:])
bpy.ops.wm.read_factory_settings(use_empty=True)
sc = bpy.context.scene
sc.render.engine = "CYCLES"
sc.cycles.device = "CPU"
sc.cycles.samples = args.samples
sc.cycles.max_bounces = 7
sc.cycles.transparent_max_bounces = 2
sc.cycles.use_denoising = args.samples >= 8
sc.cycles.use_light_tree = True
sc.render.use_persistent_data = True
sc.render.fps = MAP["fps"]
sc.render.resolution_x, sc.render.resolution_y = ((1920,1080) if args.format == "wide" else (1080,1920))
sc.render.resolution_percentage = args.scale
sc.render.image_settings.file_format = "PNG"
sc.render.image_settings.color_mode = "RGB"
sc.render.image_settings.compression = 18
sc.world = bpy.data.worlds.new("Studio / blue-black")
sc.world.use_nodes = True
sc.world.node_tree.nodes["Background"].inputs[0].default_value = (.18,.21,.26,1)
sc.world.node_tree.nodes["Background"].inputs[1].default_value = .38


def mat(name, rgb, metal=0, rough=.35, emission=0, transmission=0, brushed=False):
    m = bpy.data.materials.new(name)
    m.use_nodes = True
    p = m.node_tree.nodes.get("Principled BSDF")
    p.inputs["Base Color"].default_value = (*rgb,1)
    p.inputs["Metallic"].default_value = metal
    p.inputs["Roughness"].default_value = rough
    p.inputs["Emission Color"].default_value = (*rgb,1)
    p.inputs["Emission Strength"].default_value = emission
    p.inputs["Transmission Weight"].default_value = transmission
    if brushed:
        noise = m.node_tree.nodes.new("ShaderNodeTexNoise")
        coords = m.node_tree.nodes.new("ShaderNodeTexCoord")
        m.node_tree.links.new(coords.outputs["Object"],noise.inputs["Vector"])
        noise.inputs["Scale"].default_value = 160
        noise.inputs["Detail"].default_value = 1
        ramp = m.node_tree.nodes.new("ShaderNodeValToRGB")
        for v, el in zip((rough*.69, rough*1.27),ramp.color_ramp.elements):
            el.color = (v,v,v,1)
        m.node_tree.links.new(noise.outputs["Fac"],ramp.inputs[0])
        m.node_tree.links.new(ramp.outputs["Color"],p.inputs["Roughness"])
    return m


nickel = mat("Satin palladium",(.43,.49,.56),.86,.245,brushed=True)
silver = mat("Diamond-cut rim",(.72,.84,.92),.91,.16)
dark = mat("Blued steel",(.035,.048,.069),.78,.30)
glass = mat("Kiln amber / glass",(.91,.31,.044),0,.105,transmission=.88)
amber = mat("Amber lip",(1,.32,.046),.64,.19,.27)
jade = mat("Sea glass enamel",(.012,.31,.21),.42,.22,.16)
mint = mat("Play phosphor",(.04,.85,.41),.22,.19,1.12)
floor = mat("Lapped graphite",(.023,.030,.043),.54,.32,brushed=True)
porcelain = mat("Ivory ceramic",(.93,.90,.78),.22,.27,.34)
scribe = mat("Etched bar scale",(.39,.46,.49),.64,.32)
laser = mat("Ceramic pickup",(.98,.73,.40),.15,.20,3.2)
radii = (1.51,2.15,2.82)
groups = []
rings = []


def mesh(name, verts, faces, mats, ids=()):
    me = bpy.data.meshes.new(name)
    me.from_pydata(verts, [], faces)
    me.update()
    obj = bpy.data.objects.new(name, me)
    sc.collection.objects.link(obj)
    for m in mats:
        me.materials.append(m)
    if len(ids):
        me.polygons.foreach_set("material_index",ids)
    return obj


def empty(name):
    o = bpy.data.objects.new(name,None)
    sc.collection.objects.link(o)
    return o


def polar(radius, angle, z):
    return radius * math.sin(angle), radius * math.cos(angle), z


def lathe(name, rows, mats, ids, parent=None):
    sides = len(rows[0])
    verts = [polar(r, TAU*i/len(rows), z) for i,row in enumerate(rows) for r,z in row]
    faces = []
    slots = []
    for i in range(len(rows)):
        j = (i+1)%len(rows)
        for s,slot in enumerate(ids):
            a = (s+1)%sides
            faces.append((i*sides+s,j*sides+s,j*sides+a,i*sides+a))
            slots.append(slot)
    o = mesh(name,verts,faces,mats,slots)
    o.parent = parent
    return o


for i,take in enumerate(MAP["tracks"]):
    r = radii[i]
    root = empty(take["name"])
    groups.append(root)
    rows = []
    for pk, rms in zip(take["peak"],take["rms"]):
        inside = r - .066 - .156 * rms
        outside = r + .075 + .205 * pk
        top = .122 + .46 * pk + .22 * rms
        rows.append([(inside,.045),(inside,top-.024),(inside+.022,top),
                     (outside-.024,top),(outside,top-.025),(outside,.084),(outside-.014,.045)])
    materials = [(dark,nickel,silver),(glass,glass,amber),(jade,jade,mint)][i]
    ring = lathe("PCM / "+take["name"],rows,materials,[0,2,1,2,0,2,0],root)
    rings.append(ring)
    # Match the cap stroke while leaving a shallow PCM engraving on each edge.
    ring.shape_key_add(name="Take")
    stroke = ring.shape_key_add(name="Letter stroke")
    stroke.value=0
    for k, point in enumerate(stroke.data):
        column = k % 7
        v = point.co
        radius = math.hypot(v.x,v.y)
        edge = (-.128,-.128,-.118,.115,.128,.128,.118)[column]
        finished = r * (1 + edge) + .12 * (radius-r)
        v.x *= finished/radius
        v.y *= finished/radius
        v.z = (.012,.084,.098,.098,.083,.022,.012)[column] + .025*v.z

# One physical timing scale. Long notches are downbeats. The broad open center
# reads like the label at the center of a record, even in the opening macro.
dial = empty("Sixteen beats")
def torus_strip(name,r,w,h,m,parent):
    row = [(r-w,.007),(r-w,h),(r+w,h),(r+w,.007)]
    return lathe(name,[row]*384,[m],[0,0,0,0],parent)
torus_strip("Outer scale",3.51,.008,.018,scribe,dial)
torus_strip("Inner index",1.02,.012,.02,nickel,dial)
# The cut metal plate supports the loose takes and catches their shadows.
plate = [(0.01,-.095),(0.01,.004),(3.55,.004),(3.568,-.013),
         (3.565,-.050),(3.55,-.057),(3.55,-.083),(3.53,-.095)]
lathe("Lathed platen", [plate]*384, [floor,dark,silver], [0,0,2,1,2,1,1,1],dial)

verts, faces, slots = [], [], []
for i in range(128):
    angle = TAU*i/128
    down, beat = i%32==0, i%8==0
    r0,r1 = 3.31,3.49 if down else (3.44 if beat else 3.36)
    da = .007 if down else (.004 if beat else .0024)
    start = len(verts)
    verts.extend(polar(r,a,.009) for r,a in ((r0,angle-da),(r0,angle+da),(r1,angle+da),(r1,angle-da)))
    faces.append(tuple(range(start,start+4)))
    slots.append(int(down))
ticks = mesh("Quarter notes / subdivisions",verts,faces,[scribe,porcelain],slots)
ticks.parent = dial
bpy.ops.mesh.primitive_plane_add(size=600)
bpy.context.object.data.materials.append(mat("Cyclorama",(.014,.019,.026),.29,.42))
bpy.context.object.location.z=-.097

font_path = HERE/"inputs/fonts/InstrumentSans-Variable.ttf"
font = bpy.data.fonts.load(str(font_path))
mono = bpy.data.fonts.load("/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf")
def lettering(body,size,x,y,z,material,font=font,align="CENTER",space=1,depth=.003):
    c = bpy.data.curves.new(body,"FONT")
    c.body,c.font,c.size,c.align_x = body,font,size,align
    c.align_y = "CENTER"
    c.space_character = space
    c.extrude,c.bevel_depth = depth,depth*.3
    o = bpy.data.objects.new(body,c)
    sc.collection.objects.link(o)
    o.location=(x,y,z)
    c.materials.append(material)
    return o

for i,word in enumerate(("I","II","III","IV")):
    a=TAU*i/4
    x,y,_=polar(3.74,a,0)
    o=lettering(word,.16,x,y,-.091,scribe,mono)
    o.parent=dial
numbers = [lettering(f"0{i+1}",.49,0,.105,.024,porcelain,mono) for i in range(4)]
caption = lettering("BAR / 04",.125,0,-.22,.025,porcelain,mono,space=1.14)
for o in (*numbers,caption):
    o.parent=dial

# Physical wordmark. Use the real cap widths, substituting the two circular
# counters with the drum and key takes at their upright docking positions.
left = lettering("BLEEPL",1.55,0,0,.045,porcelain,align="LEFT",space=1.03,depth=.016)
right = lettering("P",1.55,0,0,.045,porcelain,align="LEFT",depth=.016)
bpy.context.view_layer.update()
cap_h = left.dimensions.y
# Typeset from visible ink bounds, including each font side bearing.
o_width = 1.03*cap_h
letter_gap, pair_gap = .10*cap_h, .085*cap_h
word_width = left.dimensions.x + right.dimensions.x + 2*o_width + 2*letter_gap + pair_gap
origin = -word_width/2
left.location.x = origin - min(p[0] for p in left.bound_box)
o1 = origin + left.dimensions.x + letter_gap + o_width/2
o2 = o1 + o_width + pair_gap
right.location.x = o2 + o_width/2 + letter_gap - min(p[0] for p in right.bound_box)
# Blender CENTER aligns the font em-box. Measure actual glyph cap bounds.
cap_y = sum(p[1] for p in left.bound_box)/8

arm = empty("Pickup / clock")
def blade(name,r0,r1,w,z1,m):
    v=[(x,y,z1+z) for z in (0,.026) for y in (r0,r1) for x in (-w,w)]
    f=[(0,2,3,1),(4,5,7,6),(0,1,5,4),(2,6,7,3),(0,4,6,2),(1,3,7,5)]
    ob=mesh(name,v,f,[m]); ob.parent=arm
    return ob
# A parked rim carriage carries a cantilever; its face and shoe meet in 3D.
# Only the inboard strip draws light, keeping the silver carrier identifiable.
blade("Cantilever / titanium",1.32,3.52,.039,.835,dark)
blade("Inboard read strip",1.34,3.47,.012,.864,laser)
def carriage(name,r0,r1,half_width,z0,z1,material):
    v=[(x,y,z) for z in (z0,z1) for y in (r0,r1) for x in (-half_width,half_width)]
    f=[(0,2,3,1),(4,5,7,6),(0,1,5,4),(2,6,7,3),(0,4,6,2),(1,3,7,5)]
    o=mesh(name,v,f,[material])
    o.parent=arm
    bevel=o.modifiers.new("Milled edges","BEVEL")
    bevel.width=.021
    bevel.segments=3
    return o
carriage("Sliding shoe",3.40,3.60,.12,.006,.105,nickel)
carriage("Upright",3.46,3.54,.063,.087,.86,dark)
carriage("Pivot block",3.414,3.586,.097,.782,.902,nickel)
carriage("Amber witness",3.49,3.52,.065,.903,.914,amber)
def light(name,typ,pos,color,energy,size,target=(0,0,0),size_y=None):
    d=bpy.data.lights.new(name,typ)
    o=bpy.data.objects.new(name,d)
    sc.collection.objects.link(o)
    o.location=pos
    d.energy,d.color=energy,color
    if typ=="AREA":
        d.shape="RECTANGLE"
        d.size,d.size_y=size,size_y or size
        o.rotation_euler=(Vector(target)-o.location).to_track_quat("-Z","Y").to_euler()
    else:
        d.shadow_soft_size=size
    return o
pickup=light("Pickup spill","POINT",(0,2.2,.94),(1,.55,.19),29,.26)
pickup.parent=arm
contacts=[]
for i, root in enumerate(groups):
    bpy.ops.mesh.primitive_uv_sphere_add(segments=16,ring_count=8,radius=.031)
    bead=bpy.context.object
    bead.name="Read contact / "+MAP["tracks"][i]["name"]
    bead.data.materials.append(laser)
    bead.parent=root
    contacts.append(bead)
light("Long cool strip","AREA",(-3.8,1.2,6.3),(.72,.84,1),620,5.8,size_y=1.0)
light("Warm silk","AREA",(4.5,3.7,4),(1,.68,.39),510,4.0,size_y=2.6)
light("White reflector","AREA",(2,-5,5.4),(1,.95,.86),430,5.0,size_y=2.5)

camdata=bpy.data.cameras.new("Motion control")
cam=bpy.data.objects.new("Motion control",camdata)
sc.collection.objects.link(cam)
sc.camera=cam
camdata.type="ORTHO"
def ease(x):
    x=max(0,min(x,1))
    return x*x*(3-2*x)
def lerp(a,b,t):
    return a+(b-a)*t
def curve(f,keys):
    for (a,x),(b,y) in zip(keys,keys[1:]):
        if f<=b:
            return lerp(x,y,ease((f-a)/(b-a)))
    return keys[-1][1]

# The lettering stands on a second mark upstage. The camera discovers it as
# it follows the two flying takes. Nothing switches the title's visibility.
stage_y, stage_z = 10.7, 11.9
stand = math.radians(78)
for letter in (left,right):
    letter.rotation_euler.x=stand
    letter.location.y=stage_y-cap_y*math.cos(stand)
    letter.location.z=stage_z-cap_y*math.sin(stand)
    letter.hide_render=args.hero

keylight=light("Title / overhead silk","AREA",(0,stage_y-3.8,stage_z+3.8),(1,.88,.72),0,7,(0,stage_y,stage_z),2.5)
rimlight=light("Title / edge strip","AREA",(-2,stage_y+1.7,stage_z+1.8),(.62,.85,1),0,5,(0,stage_y,stage_z),1.3)

def amplitude(take,f):
    # A short release lets a sub-frame drum attack read on a 25 fps display.
    stride=MAP["bins"]//LOOP_FRAMES
    energy=0
    for lag,weight in enumerate((1,.78,.51,.28)):
        index=((f-lag)%LOOP_FRAMES)*stride
        peak=max(take["peak"][index:index+stride])
        rms=max(take["rms"][index:index+stride])
        energy=max(energy,weight*(.40*peak+.60*rms))
    return energy

def pose(f):
    launch=ease((f-337)/40)
    release=ease((f-332)/22)
    amber.node_tree.nodes["Principled BSDF"].inputs["Emission Strength"].default_value=.27+.75*launch
    glass.node_tree.nodes["Principled BSDF"].inputs["Emission Strength"].default_value=.38*launch
    keylight.data.energy=560*launch
    rimlight.data.energy=390*launch
    for i,(root,r,take) in enumerate(zip(groups,radii,MAP["tracks"])):
        entry=take["entry_frame"]
        rings[i].hide_render=f<entry-7
        drop=(1-ease((f-entry+7)/7))*1.65 if i else 0
        pulse=0 if args.hero or f<entry else amplitude(take,f)*(1-release)
        radial=1+.033*pulse
        # Keep the highest ridge beneath the physical read arm even on attack.
        flex=1 if args.hero else 1+.36*pulse-.30*(1-release)
        root.scale=(radial,radial,flex)
        root.location=(0,0,drop-.041)
        if i:
            start=336 if i==1 else 344
            finish=409 if i==1 else 423
            travel=ease((f-start)/(finish-start))
            gather=ease((f-start)/46)
            slot=(o1,o2)[i-1]
            # Diverge on lift-off, turn edge-on to the platen, then approach
            # the two type slots from below on parallel, separated paths.
            side=(-.92 if i==1 else 1.1)*math.sin(math.pi*gather)
            size=lerp(radial,.452*cap_h/r,gather)
            root.scale=(size,size,lerp(flex,.58,gather))
            root.rotation_euler.x=stand*ease((f-start-2)/44)
            root.location=(slot*gather+side,stage_y*travel,
                           drop-.041+(stage_z+.041)*travel+1.0*math.sin(math.pi*travel))
            rings[i].data.shape_keys.key_blocks[1].value=ease((f-start-18)/50)
        bin_index=(f%LOOP_FRAMES)*MAP["bins"]//LOOP_FRAMES
        pk,rms=take["peak"][bin_index],take["rms"][bin_index]
        contacts[i].hide_render=f<entry or f>=353
        contacts[i].location=polar(r,TAU*(f%LOOP_FRAMES)/LOOP_FRAMES,.165+.46*pk+.22*rms)
        contacts[i].scale=(1-release,)*3
    # The bass stays in the machine. The pickup parks below the surface.
    arm.rotation_euler.z=-TAU*(f%LOOP_FRAMES)/LOOP_FRAMES
    arm.location.z=-1.35*release
    pickup.data.energy=29*(1-release)
    for i,number in enumerate(numbers):
        number.hide_render=f<60 or f>=345 or i!=(f//FRAMES_PER_BAR)%4
    caption.hide_render=f<60 or f>=345
    tilt=curve(f,[(0,68),(75,59),(119,54),(195,48),(250,63),(312,58),(337,45),(365,51),(407,78)])
    az=curve(f,[(0,-32),(120,-16),(245,15),(337,20),(365,0)])
    track=.62*ease((f-336)/73)+.38*ease((f-344)/79)
    lead=math.sin(math.pi*track)
    target=Vector((curve(f,[(0,.22),(105,0)])+ .67*lead*(1-ease((f-362)/16)),
                   curve(f,[(0,1.48),(75,.84),(120,0)]) + stage_y*track,
                   curve(f,[(0,.24),(115,.13)]) + (stage_z-.13)*track+.25*lead))
    tilt,az=map(math.radians,(tilt,az))
    direction=Vector((math.sin(az)*math.sin(tilt),-math.cos(az)*math.sin(tilt),math.cos(tilt)))
    cam.location=target+direction*15
    cam.rotation_euler=(-direction).to_track_quat("-Z","Y").to_euler()
    w=curve(f,[(0,2.6),(74,4.3),(138,7.1),(229,6.55),(270,8.7),(346,10.65),(356,12.6),(365,13.0),(384,11.4),(426,word_width*1.30)])
    if args.format=="tall":
        w*=curve(f,[(0,1.04),(140,1.40),(346,1.53),(365,1.25),(384,1.22)])
        w=lerp(w,word_width/(.85*9/16),ease((f-363)/63))
    camdata.ortho_scale=w
    if args.hero:
        # The poster keeps the art-directed beauty exposure from the first cut.
        tilt,az=map(math.radians,(curve(f,[(250,63),(312,58)]),curve(f,[(245,15),(349,22)])))
        d=Vector((math.sin(az)*math.sin(tilt),-math.cos(az)*math.sin(tilt),math.cos(tilt)))
        cam.location=Vector((0,0,curve(f,[(115,.13),(349,.12)])))+d*15
        cam.rotation_euler=(-d).to_track_quat("-Z","Y").to_euler()
        camdata.ortho_scale=11.42
        camdata.shift_x=-.157
        camdata.shift_y=.017

def plate_frames():
    # Once the rings leave the table they uncover long strips of the studio
    # behind them. Capture every frame in both gates to retain antialiased
    # silhouettes and moving reflections against that dark field.
    flight=set(range(336,432))
    if args.format=="tall":
        return sorted(flight | {0,12,24,48,72,96,112,113,119,120,144,180,
                                216,232,233,239,240,264,300})
    events=flight | {0,4,8,16,20,30,42}
    events.update(range(0,336,12))
    for entry in (120,240):
        events.update(range(entry-8,entry+3,2))
        events.update((entry-7,entry-1,entry))
    for bar in range(60,336,60):
        events.update((bar-1,bar))
    return sorted(events)

dest=OUT/args.dir/args.format
dest.mkdir(parents=True,exist_ok=True)
def finish_batch():
    # With no audio socket Cycles in this sandbox can hang in libpulse teardown
    # even under -b -noaudio. This entry point is a single-use background job.
    # All render writes and geometry files have already been closed or renamed.
    sys.stdout.flush()
    sys.stderr.flush()
    os._exit(0)


if args.audit:
    import hashlib
    import numpy as np
    import wave
    pose(302)
    bpy.context.view_layer.update()
    deps=bpy.context.evaluated_depsgraph_get()
    for i,take in enumerate(MAP["tracks"]):
        path=OUT/take["file"]
        assert hashlib.sha256(path.read_bytes()).hexdigest()==take["sha256"]
        with wave.open(str(path),"rb") as wav:
            pcm=np.frombuffer(wav.readframes(wav.getnframes()),"<i2").astype(float)
        windows=pcm.reshape(MAP["bins"],-1,2)
        stereo_power=np.mean(windows**2,axis=2)
        pk=np.sqrt(stereo_power.max(axis=1)); pk/=pk.max()
        rms=np.sqrt(stereo_power.mean(axis=1)); rms/=rms.max()
        coords=np.empty(len(rings[i].data.vertices)*3,dtype="f")
        evaluated=rings[i].evaluated_get(deps)
        actual_mesh=evaluated.to_mesh()
        actual_mesh.vertices.foreach_get("co",coords)
        evaluated.to_mesh_clear()
        sections=coords.reshape(MAP["bins"],7,3)
        actual_r=np.linalg.norm(sections[:,4,:2],axis=1)
        actual_z=sections[:,2,2]
        np.testing.assert_allclose(actual_r,radii[i]+.075+.205*pk,atol=1e-6)
        np.testing.assert_allclose(actual_z,.122+.46*pk+.22*rms,atol=1e-6)
        print("Verified WAV against mesh:",take["name"],len(pk),"cross sections")
    finish_batch()

if args.project or args.topology_only:
    import numpy as np
    import struct
    base=OUT/"projection"/args.format
    base.mkdir(parents=True,exist_ok=True)
    pose(0)
    bpy.context.view_layer.update()
    deps=bpy.context.evaluated_depsgraph_get()
    pieces=[]
    triangles=[]
    ids=[]
    sections=[]
    offset=0
    cast={}
    for obj in sc.objects:
        if obj.type not in ("MESH","FONT"):
            continue
        evaluated=obj.evaluated_get(deps)
        me=evaluated.to_mesh()
        me.calc_loop_triangles()
        xyz=np.empty((len(me.vertices),3),np.float32)
        me.vertices.foreach_get("co",xyz.ravel())
        tris=np.empty((len(me.loop_triangles),3),np.int32)
        me.loop_triangles.foreach_get("vertices",tris.ravel())
        ids.append(np.full(len(tris),len(pieces),dtype=np.int32))
        # A per-face address identifies the actual PCM section the pickup reads.
        address=np.full(len(tris),-1,dtype=np.int32)
        if obj in rings:
            face_index=np.empty(len(tris),np.int32)
            me.loop_triangles.foreach_get("polygon_index",face_index)
            address=rings.index(obj)*MAP["bins"]+face_index//7
        sections.append(address)
        cast[obj.name]=[offset,offset+len(xyz)]
        triangles.append(tris+offset)
        offset+=len(xyz)
        delta=None
        if obj in rings:
            widened=xyz.copy()
            obj.data.shape_keys.key_blocks[1].data.foreach_get("co",widened.ravel())
            delta=widened-xyz
        pieces.append((obj,xyz,delta))
        evaluated.to_mesh_clear()
    triangles=np.concatenate(triangles)
    with (base/"topology.bin").open("wb") as out:
        out.write(struct.pack("<II",offset,len(triangles)))
        triangles.tofile(out)
    (base/"cast.json").write_text(json.dumps(cast))
    np.concatenate(ids).tofile(base/"parts.bin")
    np.concatenate(sections).tofile(base/"sections.bin")
    bins=MAP["bins"]
    wakes=np.zeros((432,len(rings),bins),np.float32)
    for f,row in enumerate(wakes):
        # A short hot leading edge and a longer trailing decay travel clockwise.
        # Brightness and length follow the same stem as the press this hits.
        for i,take in enumerate(MAP["tracks"]):
            if take["entry_frame"]<=f<354:
                strength=amplitude(take,f)
                lag=(f/LOOP_FRAMES*bins-np.arange(bins)-.5+bins/2)%bins-bins/2
                trail=np.where(lag>=0, np.exp(-abs(lag)/(14+29*strength)),
                               np.exp(-abs(lag)/3))
                row[i]=trail*(.44+.50*strength)*(1-ease((f-332)/22))
    wakes.tofile(base/"wake.bin")
    if args.topology_only:
        finish_batch()
    render_width,render_height=sc.render.resolution_x,sc.render.resolution_y
    for f in range(432):
        pose(f)
        bpy.context.view_layer.update()
        view=cam.matrix_world.inverted()
        zoom=max(render_width,render_height)/camdata.ortho_scale
        with (base/f"{f:04d}.bin").open("wb") as out:
            for obj,xyz,delta in pieces:
                m=np.array(view@obj.matrix_world,dtype=np.float32)
                current=xyz if delta is None else xyz+delta*obj.data.shape_keys.key_blocks[1].value
                screen=current@m[:3,:3].T+m[:3,3]
                screen[:,0]=render_width*.5+screen[:,0]*zoom-.5
                screen[:,1]=render_height*.5-screen[:,1]*zoom-.5
                if obj.hide_render:
                    screen[:,2]=-1e5
                screen.tofile(out)
    print("Projected",len(pieces),"objects at all moving frames")
    finish_batch()

indices=[]
if args.frames=="plates":
    indices=plate_frames()
for part in ([] if args.frames=="plates" else args.frames.split(",")):
    ns=[int(n) for n in part.split("-")]
    indices.extend(range(ns[0],ns[-1]+1))
for f in indices:
    path=dest/f"{f:04d}.png"
    if path.is_file():
        continue
    pose(f)
    sc.render.filepath=str(path.with_suffix(".part.png"))
    bpy.ops.render.render(write_still=True)
    os.replace(sc.render.filepath,path)
    print("FRAME",f,flush=True)

finish_batch()

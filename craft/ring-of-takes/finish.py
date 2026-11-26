"""Title and mux the projected Blender film, preserving the sample-aligned clock."""
import argparse
import json
import os
from pathlib import Path
import subprocess

HERE = Path(__file__).resolve().parent
ROOT = HERE
OUT = Path(os.getenv("PROMO_WORK", ROOT / "output"))
FONTS = HERE / "inputs/fonts"


def timestamp(seconds):
    cs = round(seconds * 100)
    return f"0:{cs//6000:02}:{cs//100%60:02}.{cs%100:02}"


def titles(fmt, score):
    wide = fmt == "wide"
    w, h = (1920, 1080) if wide else (1080, 1920)
    x, y = (98, 842) if wide else (85, 1402)
    path = OUT / f"{fmt}.ass"
    header = f"""[Script Info]
ScriptType: v4.00+
PlayResX: {w}
PlayResY: {h}
ScaledBorderAndShadow: yes
WrapStyle: 2

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Take,JetBrainsMono Nerd Font,30,&H00F1E7D9,&H00FFFFFF,&HB51B1814,&H55171310,0,0,0,0,100,100,3,0,1,0.8,1.4,7,0,0,0,1
Style: Voice,Instrument Sans,76,&H00F2F2F3,&H00FFFFFF,&HB51B1814,&H55171310,-1,0,0,0,100,100,-1,0,1,0.9,1.6,7,0,0,0,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
"""
    lines = []
    for i, take in enumerate(score["tracks"]):
        start = take["entry_frame"] / score["fps"]
        end = start + 3.08
        for style, dy, words in (("Take", 0, f"TAKE 0{i+1}"), ("Voice", 36, take["name"])):
            # Place both lines in title-safe space and leave the second half
            # of each shot clear for the physical object.
            tags = r"{\an7\pos(%d,%d)\fad(120,360)\blur0.45}" % (x, y+dy)
            lines.append(f"Dialogue: 0,{timestamp(start)},{timestamp(end)},{style},,0,0,0,,{tags}{words}")
    path.write_text(header+"\n".join(lines)+"\n")
    return path


def encode(fmt, first=0, last=479):
    score = json.loads((OUT / "map.json").read_text())
    wide = fmt == "wide"
    full = first == 0 and last == score["frames"]-1
    dest = ROOT / "deliverables" / ("BleepLoop-Ring-of-Takes-" + ("16x9" if wide else "9x16") + ".mp4")
    if not full:
        dest = OUT / f"preview-{fmt}-{first}-{last}.mp4"
    dest.parent.mkdir(exist_ok=True)
    tmp = dest.with_suffix(".part.mp4")
    ass = titles(fmt, score)
    filters = (f"setpts=PTS+{first}/25/TB,ass=filename={ass}:fontsdir={FONTS},"
               f"setpts=PTS-STARTPTS,scale=out_color_matrix=bt709:out_range=tv,format=yuv420p")
    with (OUT / f"projection-{fmt}.log").open("w") as log:
        composite = subprocess.Popen([OUT/"composite",OUT,fmt,str(first),str(last),"plates"],
                stdout=subprocess.PIPE,stderr=log,env={**os.environ,"OMP_NUM_THREADS":"4"})
        opts = ["ffmpeg","-nostdin","-y","-loglevel","error","-f","rawvideo","-pixel_format","rgb24",
                "-video_size",("1920x1080" if wide else "1080x1920"),"-framerate","25","-i","pipe:0",
                "-ss",str(first/25),"-i",OUT/"master.wav","-t",str((last-first+1)/25),
                "-map","0:v:0","-map","1:a:0","-filter_threads","3","-vf",filters,
                "-c:v","libx264","-preset","fast","-crf","17","-threads","4",
                "-color_primaries","bt709","-colorspace","bt709","-color_trc","bt709",
                "-c:a","aac","-b:a","256k","-movflags","+faststart",
                "-metadata","title=BleepLoop — Ring of Takes",tmp]
        try:
            subprocess.run(opts,stdin=composite.stdout,check=True)
        finally:
            composite.stdout.close()
            status = composite.wait()
        if status:
            raise RuntimeError(f"compositor exited {status}; see {log.name}")
    tmp.replace(dest)
    print(dest, flush=True)


def hero():
    dest = ROOT / "deliverables/BleepLoop-Ring-of-Takes-Hero.png"
    # The poster has its own camera, with open space for the brand on the left.
    ass = OUT/"hero.ass"
    header=titles("wide",{"tracks":[]}).read_text()
    # Restrained tracking preserves the compact product signature at thumbnail size.
    header += (r"Dialogue: 0,0:00:00.00,0:00:01.00,Take,,0,0,0,,{\an7\pos(96,440)\fs25\fsp4}RING OF TAKES" + "\n" +
               r"Dialogue: 0,0:00:00.00,0:00:01.00,Voice,,0,0,0,,{\an7\pos(92,475)\fs91\fsp1}BLEEPLOOP" + "\n")
    ass.write_text(header)
    dest.parent.mkdir(exist_ok=True)
    tmp=dest.with_suffix(".part.png")
    subprocess.run(["ffmpeg","-y","-nostdin","-v","error","-i",OUT/"poster/wide/0302.png",
                    "-vf",f"ass=filename={ass}:fontsdir={FONTS}","-frames:v","1",tmp],check=True)
    tmp.replace(dest)
    print(dest)

if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("format",choices=("wide","tall","hero"))
    p.add_argument("--first",type=int,default=0)
    p.add_argument("--last",type=int,default=479)
    opt=p.parse_args()
    if opt.format=="hero":
        hero()
    else:
        encode(opt.format,opt.first,opt.last)

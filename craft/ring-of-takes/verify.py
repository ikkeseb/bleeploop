"""Check both actual delivery files against the four-bar session and stereo master."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import wave

import numpy as np

ROOT = Path(__file__).resolve().parent
OUT = Path(os.getenv("PROMO_WORK", ROOT / "output"))
DELIVERY = ROOT/"deliverables"


def probe(path):
    command = ["ffprobe","-v","error","-show_streams","-show_format","-of","json",path]
    return json.loads(subprocess.check_output(command))


def run():
    score = json.loads((OUT/"map.json").read_text())
    with wave.open(str(OUT/"master.wav"),"rb") as w:
        rate = w.getframerate()
        expected = np.frombuffer(w.readframes(w.getnframes()),"<i2").reshape(-1,2)/32768
    assert rate==44100 and len(expected)==846720
    entries = [t["entry_frame"] for t in score["tracks"]]
    assert entries==[0,120,240] and all(f%15==0 for f in entries)
    rebuilt=np.zeros_like(expected)
    for take in score["tracks"]:
        path=OUT/take["file"]
        assert hashlib.sha256(path.read_bytes()).hexdigest()==take["sha256"]
        with wave.open(str(path),"rb") as source:
            assert source.getframerate()==rate and source.getnframes()==423360
            stem=np.frombuffer(source.readframes(source.getnframes()),"<i2").reshape(-1,2)/32767
        entry=take["entry_frame"]*rate//score["fps"]
        rebuilt[entry:]+=stem[np.arange(entry,len(rebuilt))%len(stem)]
    tail=int(.34*rate)
    rebuilt[-tail:]*=np.linspace(1,0,tail)[:,None]**1.4
    expected_pcm=np.rint(rebuilt*(.93/np.max(np.abs(rebuilt)))*32767)
    np.testing.assert_array_equal(expected*32768,expected_pcm)
    print("All master PCM samples match the three takes at the specified frame entries.")
    for fmt,w,h in (("wide",1920,1080),("tall",1080,1920)):
        for frame in (0,120,240,349,380,414,431):
            points=np.fromfile(OUT/"projection"/fmt/f"{frame:04d}.bin","<f4").reshape(-1,3)
            visible=points[points[:,2]>-9999]
            assert np.isfinite(visible).all()
            assert visible[:,0].min()<0 and visible[:,0].max()>w
            assert visible[:,1].min()<0 and visible[:,1].max()>h
        # Check every in-flight pose against the solid type. Conservative ink
        # boxes catch a rim, arm or numeral passing through either letter block.
        directory=OUT/"projection"/fmt
        cast=json.loads((directory/"cast.json").read_text())
        drum,keys=("PCM / "+track["name"] for track in score["tracks"][1:])
        machinery=("Lathed platen","Cantilever / titanium","01","02","03","04","BAR / 04")
        for frame in range(345,432):
            points=np.fromfile(directory/f"{frame:04d}.bin","<f4").reshape(-1,3)
            def bounds(name):
                group=points[slice(*cast[name])]
                return group[:,:2].min(0),group[:,:2].max(0)
            for name in (drum,keys,*machinery):
                if points[cast[name][0],2]<-9999:
                    continue
                low,high=bounds(name)
                if name in (drum,keys):
                    assert min(*low,w-high[0],h-high[1])>0, (fmt,frame,name)
                for letter in ("BLEEPL","P"):
                    a,b=bounds(letter)
                    assert min(np.minimum(high,b)-np.maximum(low,a))<=0, (fmt,frame,name,letter)
        if fmt=="tall":
            left=bounds("BLEEPL")[0][0]
            right=bounds("P")[1][0]
            assert .84 < (right-left)/w < .86 and abs(left-(w-right))<3
            print(f"Phone title spans {(right-left)/w:.1%} of the frame.")
        for frame in range(336,432):
            assert (OUT/"plates"/fmt/f"{frame:04d}.png").is_file()
    previous_audio = None
    for aspect, width, height in (("16x9",1920,1080),("9x16",1080,1920)):
        path = DELIVERY/f"BleepLoop-Ring-of-Takes-{aspect}.mp4"
        streams = probe(path)["streams"]
        video, sound = streams
        assert (video["codec_name"],video["pix_fmt"],video["width"],video["height"])==("h264","yuv420p",width,height)
        assert video["avg_frame_rate"]=="25/1" and int(video["nb_frames"])==480
        assert sound["codec_name"]=="aac" and sound["channels"]==2 and int(sound["sample_rate"])==rate
        for stream in streams:
            assert float(stream["start_time"])==0 and abs(float(stream["duration"])-19.2)<.0001
        subprocess.run(["ffmpeg","-v","error","-xerror","-threads","2","-i",path,
                        "-map","0:v","-f","null","-"],check=True)
        decoded = subprocess.check_output(["ffmpeg","-v","error","-i",path,"-map","0:a","-f","f32le","-"])
        if previous_audio is not None:
            assert decoded==previous_audio
        previous_audio=decoded
        # Subtitling-free exposures must survive the actual delivery path.
        # This catches a wrong closing shot or a displaced portrait crop.
        directory="wide" if aspect=="16x9" else "tall"
        small="320:180" if directory=="wide" else "180:320"
        for frame in (336,363,375,393,405,431):
            def thumbnail(source,seek):
                opts=["ffmpeg","-v","error","-threads","2",*seek,"-i",source,"-frames:v","1",
                      "-vf",f"scale={small}:flags=area","-f","rawvideo","-pix_fmt","rgb24","-"]
                return np.frombuffer(subprocess.check_output(opts),np.uint8).astype(float)
            encoded=thumbnail(path,["-ss",str(frame/25)])
            plate=thumbnail(OUT/"plates"/directory/f"{frame:04d}.png",[])
            assert len(encoded)==len(plate)==320*180*3
            mae=np.mean(np.abs(encoded-plate))
            assert mae<4.0, (aspect,frame,mae)
            print(f"{aspect} frame {frame}: encoded vs Blender mean error {mae:.2f}/255")
        actual=np.frombuffer(decoded,"<f4").reshape(-1,2)[:len(expected)]
        # Include a pre-entry and a post-entry block for every addition. A frame
        # slip is 1,764 samples, easily distinguished from lossy coding error.
        for frame in [0,108,120,228,240,430]:
            start=frame*rate//25
            a=actual[start:start+rate,:].ravel()
            e=expected[start:start+rate,:].ravel()
            assert np.corrcoef(a,e)[0,1]>.995
        snr=10*np.log10(np.mean(expected**2)/np.mean((expected-actual)**2))
        assert snr>24
        print(f"{aspect}: {width}x{height}, 480 frames, all frames decode, 19.200s; AAC/master SNR {snr:.1f} dB")
    hero=probe(DELIVERY/"BleepLoop-Ring-of-Takes-Hero.png")["streams"][0]
    assert (hero["width"],hero["height"],hero["codec_name"])==(1920,1080,"png")
    print("The stereo decodes match across both edits; hero is a 1920x1080 PNG.")


if __name__=="__main__":
    run()

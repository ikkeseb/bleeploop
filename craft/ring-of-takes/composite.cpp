// Move Blender beauty plates on their actual triangle surfaces between exposures.
// Orthographic cameras keep barycentric coordinates affine in both images.
#include <png.h>
#include <algorithm>
#include <array>
#include <cmath>
#include <cstdio>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <stdexcept>
#include <string>
#include <vector>

namespace fs = std::filesystem;
using RGB = std::array<float, 3>;
struct Vec { float x, y, z; };
struct Tri { int a, b, c; };
struct Hit { int tri = -1; float b = 0, c = 0, z = -1e8f; };
struct Topology { int vertices; std::vector<Tri> faces; std::vector<int> part; };
struct Surface { std::vector<Vec> points; std::vector<Hit> pixels; };
struct Photo { std::vector<unsigned char> data; Surface geometry; };
struct Size { int w, h; };
struct Sample { RGB color{}; float visibility=0; };

std::string stem(int f) {
    char value[12];
    std::snprintf(value, sizeof(value), "%04d", f);
    return value;
}
template<class T> void read_items(std::ifstream &f, T *target, size_t n) {
    if (!f.read(reinterpret_cast<char *>(target), n * sizeof(T)))
        throw std::runtime_error("incomplete geometry file");
}
Topology topology(const fs::path &file) {
    std::ifstream f(file, std::ios::binary);
    int count[2]; read_items(f,count,2);
    Topology t{count[0], std::vector<Tri>(count[1]), std::vector<int>(count[1])};
    std::ifstream ids(file.parent_path()/"parts.bin",std::ios::binary);
    read_items(ids,t.part.data(),t.part.size());
    read_items(f,t.faces.data(),t.faces.size());
    return t;
}
std::vector<unsigned char> read_png(const fs::path &path, Size size) {
    png_image im{}; im.version = PNG_IMAGE_VERSION;
    if (!png_image_begin_read_from_file(&im,path.c_str()))
        throw std::runtime_error("missing plate " + path.string());
    if (int(im.width)!=size.w || int(im.height)!=size.h)
        throw std::runtime_error("unexpected plate dimensions");
    im.format = PNG_FORMAT_RGB;
    std::vector<unsigned char> rgb(PNG_IMAGE_SIZE(im));
    if (!png_image_finish_read(&im,nullptr,rgb.data(),0,nullptr))
        throw std::runtime_error("could not decode plate");
    png_image_free(&im);
    return rgb;
}
Surface surface(const fs::path &base,int frame,const Topology &top,Size s) {
    Surface g;
    g.points.resize(top.vertices);
    std::ifstream in(base/(stem(std::min(frame,431))+".bin"),std::ios::binary);
    read_items(in,g.points.data(),g.points.size());
    g.pixels.resize(s.w*s.h);
    for (size_t k=0; k<top.faces.size(); ++k) {
        auto t=top.faces[k];
        Vec a=g.points[t.a], b=g.points[t.b], c=g.points[t.c];
        if (a.z < -9999) continue;
        float bx=b.x-a.x, by=b.y-a.y, cx=c.x-a.x, cy=c.y-a.y;
        float determinant=bx*cy-by*cx;
        if (std::abs(determinant)<1e-7f) continue;
        int left=std::max(0,int(std::ceil(std::min({a.x,b.x,c.x}))));
        int right=std::min(s.w-1,int(std::floor(std::max({a.x,b.x,c.x}))));
        int top_y=std::max(0,int(std::ceil(std::min({a.y,b.y,c.y}))));
        int bottom=std::min(s.h-1,int(std::floor(std::max({a.y,b.y,c.y}))));
        float inv=1/determinant;
        for (int y=top_y; y<=bottom; ++y) for (int x=left; x<=right; ++x) {
            float b_weight=((x-a.x)*cy-(y-a.y)*cx)*inv;
            float c_weight=(bx*(y-a.y)-by*(x-a.x))*inv;
            if (std::min({b_weight,c_weight,1-b_weight-c_weight}) < -1e-5f) continue;
            float z=a.z+(b.z-a.z)*b_weight+(c.z-a.z)*c_weight;
            Hit &h=g.pixels[y*s.w+x];
            if (z>h.z) h={int(k),b_weight,c_weight,z};
        }
    }
    return g;
}
Sample photograph(const Photo &p, const Tri &face, const Hit &hit, Size s, const Topology &top, bool recover=false) {
    auto a=p.geometry.points[face.a], b=p.geometry.points[face.b], c=p.geometry.points[face.c];
    float x=a.x+(b.x-a.x)*hit.b+(c.x-a.x)*hit.c;
    float y=a.y+(b.y-a.y)*hit.b+(c.y-a.y)*hit.c;
    float z=a.z+(b.z-a.z)*hit.b+(c.z-a.z)*hit.c;
    int ix=int(std::floor(x)), iy=int(std::floor(y));
    Sample out;
    RGB plain{};
    float total=0;
    for (int dy=0;dy<2;++dy) for(int dx=0;dx<2;++dx) {
        int px=std::clamp(ix+dx,0,s.w-1), py=std::clamp(iy+dy,0,s.h-1);
        int index=py*s.w+px;
        float weight=(dx?x-ix:1-(x-ix))*(dy?y-iy:1-(y-iy));
        const Hit &source_hit=p.geometry.pixels[index];
        // Depth rejects a foreground tooth, shoe, or letter blocking this point.
        float depth_error=std::abs(source_hit.z-z);
        float visible=source_hit.tri==hit.tri?1.f:std::clamp(1.f-(depth_error-.012f)*28,0.f,1.f);
        if (source_hit.tri<0 || top.part[source_hit.tri]!=top.part[hit.tri]) visible=0;
        float outside=std::max({-.5f-x,x-s.w+.5f,-.5f-y,y-s.h+.5f,0.f});
        if(outside>0) {
            // The extra portrait field often ends in unbroken studio floor.
            // Extend the visible face smoothly past the edge of this exposure.
            visible=(s.h>s.w && source_hit.tri==hit.tri)?1.f/(1.f+.012f*outside):0.f;
        }
        if(z < -9999) visible=0;
        for(int ch=0;ch<3;++ch) {
            plain[ch]+=weight*p.data[3*index+ch];
            out.color[ch]+=weight*visible*p.data[3*index+ch];
        }
        total+=weight*visible;
    }
    for (int ch=0;ch<3;++ch) out.color[ch]=total>1e-4f?out.color[ch]/total:plain[ch];
    if(recover && total<.001f && z>-9999) {
        // A shoulder can uncover an area hidden in both adjacent exposures.
        // Borrow from the nearest visible pixel of the same physical part.
        for(int radius:{1,2,4,8,16,32,64}) {
            float best=1e8f;
            for(int yy:{-radius,0,radius}) for(int xx:{-radius,0,radius}) {
                int px=std::clamp(ix+xx,0,s.w-1), py=std::clamp(iy+yy,0,s.h-1);
                int at=py*s.w+px;
                auto h=p.geometry.pixels[at];
                if(h.tri<0 || top.part[h.tri]!=top.part[hit.tri]) continue;
                float error=std::abs(h.z-z);
                if(error>=best) continue;
                best=error;
                for(int ch=0;ch<3;++ch) out.color[ch]=p.data[3*at+ch];
            }
            if(best<1e7f) { total=.4f/radius; break; }
        }
    }
    out.visibility=total;
    return out;
}
std::vector<unsigned char> between(const Photo &a,const Photo &b,const Surface &target,
                                   const Topology &top, Size s, float mix, int frame) {
    std::vector<unsigned char> out(s.w*s.h*3);
    long unresolved=0;
    #pragma omp parallel for reduction(+:unresolved)
    for (int i=0;i<s.w*s.h;++i) {
        const Hit &h=target.pixels[i];
        if (h.tri<0) {
            for(int ch=0;ch<3;++ch) out[i*3+ch]=a.data[i*3+ch];
            ++unresolved;
            continue;
        }
        const Tri &f=top.faces[h.tri];
        Sample first=photograph(a,f,h,s,top), last=photograph(b,f,h,s,top);
        float w1=(1-mix)*first.visibility, w2=mix*last.visibility;
        if (w1+w2<.001f) {
            first=photograph(a,f,h,s,top,true); last=photograph(b,f,h,s,top,true);
            w1=(1-mix)*first.visibility; w2=mix*last.visibility;
            if(w1+w2<1e-6f) { w1=1-mix; w2=mix; ++unresolved; }
        }
        for(int ch=0;ch<3;++ch)
            out[i*3+ch]=std::clamp(std::lround((w1*first.color[ch]+w2*last.color[ch])/(w1+w2)),0l,255l);
    }
    std::cerr<<frame<<" uncovered "<<double(unresolved)/(s.w*s.h)<<"\n";
    return out;
}
void merge_shared_gate(std::vector<unsigned char> &phone,const Surface &tall,
                       const fs::path &out,int frame) {
    const Size wide{1920,1080}, vertical{1080,1920};
    auto name=stem(std::min(frame,431));
    std::ifstream geometry(out/"projection"/"wide"/(name+".bin"),std::ios::binary);
    // The two ortho cameras have identical orientation and center. Infer the
    // ratio from a projected vertex well away from the optical axis.
    float ratio=0;
    for(size_t i=0;i<tall.points.size();++i) {
        Vec reference; read_items(geometry,&reference,1);
        float x=reference.x-(wide.w-1)*.5f;
        if(std::abs(x)>100 && reference.z>-9999 && tall.points[i].z>-9999) {
            ratio=(tall.points[i].x-(vertical.w-1)*.5f)/x; break;
        }
    }
    if(!(ratio>.30f && ratio<1.1f)) throw std::runtime_error("inconsistent camera gates");
    std::ifstream input(out/"clean-wide"/(name+".rgb"),std::ios::binary);
    std::vector<unsigned char> pixels(wide.w*wide.h*3);
    read_items(input,pixels.data(),pixels.size());
    #pragma omp parallel for
    for(int y=0;y<vertical.h;++y) {
        float sy=(y-(vertical.h-1)*.5f)/ratio+(wide.h-1)*.5f;
        if(sy<1 || sy>wide.h-2) continue;
        int iy=int(sy);
        for(int x=0;x<vertical.w;++x) {
            float sx=(x-(vertical.w-1)*.5f)/ratio+(wide.w-1)*.5f;
            if(sx<1 || sx>wide.w-2) continue;
            int ix=int(sx);
            float alpha=std::clamp(std::min({sx,sy,wide.w-1-sx,wide.h-1-sy})/28.f,0.f,1.f);
            for(int ch=0;ch<3;++ch) {
                auto at=[&](int dx,int dy){return pixels[((iy+dy)*wide.w+ix+dx)*3+ch];};
                float t=sx-ix,u=sy-iy;
                float color=(1-u)*((1-t)*at(0,0)+t*at(1,0))+u*((1-t)*at(0,1)+t*at(1,1));
                auto &dst=phone[(y*vertical.w+x)*3+ch];
                dst=std::lround((1-alpha)*dst+alpha*color);
            }
        }
    }
}
// Each addressed surface is the projected mesh of one take. Resolve light
// after the beauty reconstruction so its leading edge follows every audio
// frame, including the fractions of a turn between ray-traced exposures.
struct PickupLight {
    static constexpr int bins=960;
    std::vector<int> section;
    std::vector<float> envelope;
    PickupLight(const fs::path &directory,size_t count):section(count),envelope(432*3*bins) {
        std::ifstream ids(directory/"sections.bin",std::ios::binary);
        std::ifstream wake(directory/"wake.bin",std::ios::binary);
        read_items(ids,section.data(),count);
        read_items(wake,envelope.data(),envelope.size());
    }
    void expose(std::vector<unsigned char> &pixels,const Surface &surface,int frame) const {
        const RGB tint[3]={{.69f,.89f,1.f},{1.f,.69f,.25f},{.33f,1.f,.67f}};
        const float *row=envelope.data()+frame*3*bins;
        #pragma omp parallel for
        for(size_t i=0;i<surface.pixels.size();++i) {
            int tri=surface.pixels[i].tri;
            if(tri<0) continue;
            int k=section[tri];
            if(k<0 || row[k]<.003f) continue;
            for(int ch=0;ch<3;++ch) {
                auto &v=pixels[i*3+ch];
                v=std::lround(v+(255-v)*tint[k/bins][ch]*row[k]);
            }
        }
    }
};
int main(int argc,char **argv) try {
    if(argc!=6) throw std::runtime_error("usage: composite OUTPUT wide|tall FIRST LAST STEP-PLATE-DIR");
    fs::path out=argv[1], plates=out/argv[5]/argv[2], geom=out/"projection"/argv[2];
    Size s=std::string(argv[2])=="wide"?Size{1920,1080}:Size{1080,1920};
    Topology top=topology(geom/"topology.bin");
    PickupLight pickup(geom,top.faces.size());
    std::vector<int> exposures;
    for(const auto &file:fs::directory_iterator(plates))
        if(file.path().extension()==".png" && file.path().stem().string().size()==4)
            exposures.push_back(std::stoi(file.path().stem().string()));
    std::sort(exposures.begin(),exposures.end());
    int first=std::stoi(argv[3]),last=std::stoi(argv[4]);
    if(exposures.empty() || first<exposures.front() || std::min(last,431)>exposures.back())
        throw std::runtime_error("required exposure missing");
    auto load=[&](int f){return Photo{read_png(plates/(stem(f)+".png"),s),surface(geom,f,top,s)};};
    bool wide=std::string(argv[2])=="wide";
    bool cache=wide && first==0 && last==479;
    if(cache) fs::create_directories(out/"clean-wide");
    int l=-1,r=-1; Photo left,right;
    std::vector<unsigned char> held_title;
    for(int frame=first;frame<=last;++frame) {
        if(!wide && frame>431 && !held_title.empty()) {
            if(std::fwrite(held_title.data(),1,held_title.size(),stdout)!=held_title.size()) return 2;
            continue;
        }
        int f=std::min(frame,431);
        auto hi=std::lower_bound(exposures.begin(),exposures.end(),f);
        int next=*hi, prev=next==f?f:*(hi-1);
        if(prev!=l) { left=prev==r?std::move(right):load(prev); l=prev; r=-1; }
        std::vector<unsigned char> interpolated;
        const std::vector<unsigned char> *pixels=&left.data;
        Surface target;
        if(f>=336 && prev!=next) throw std::runtime_error("missing native flight exposure");
        if(prev!=next) {
            if(next!=r) {right=load(next); r=next;}
            target=surface(geom,f,top,s);
            interpolated=between(left,right,target,top,s,float(f-prev)/(next-prev),f);
            if(!wide) merge_shared_gate(interpolated,target,out,f);
            pixels=&interpolated;
        } else if(!wide && f<336) {
            // Keep the center's sampling and texture continuous at exposures too.
            interpolated=left.data;
            merge_shared_gate(interpolated,left.geometry,out,f);
            pixels=&interpolated;

        }
        if(!wide && f==431) held_title=*pixels;
        if(cache && frame<=431) {
            auto name=out/"clean-wide"/(stem(f)+".rgb");
            std::ofstream raw(name,std::ios::binary);
            if(!raw.write(reinterpret_cast<const char *>(pixels->data()),pixels->size()))
                throw std::runtime_error("could not cache the wide camera gate");
        }
        if(f<354) {
            if(pixels!=&interpolated) interpolated=*pixels;
            pickup.expose(interpolated,prev==next?left.geometry:target,f);
            pixels=&interpolated;
        }
        if(std::fwrite(pixels->data(),1,pixels->size(),stdout)!=pixels->size()) return 2;
    }
    return 0;
} catch(const std::exception &e) {
    std::cerr<<e.what()<<"\n";
    return 1;
}

#!/usr/bin/env bash
# Synthetic test-video set for FramePlayer: every projection x stereo layout
# x codec, with filename tokens matching fp-core's detector, burned-in labels
# (eye, projection, codec), calibration grids, a running timecode and a
# moving marker for judder checks. Plus HDR10/HLG, multichannel/ambisonic
# audio, subtitle and container-metadata cases.
#
#   tools/gen-test-videos.sh [--out DIR] [--eye PX] [--duration S] [--fps N]
#                            [--codecs "h264 hevc vp9 av1"] [--quick] [--full]
#
#   --eye PX     per-eye height in pixels (default 1024; --full = 4096, i.e. 8K SBS)
#   --quick      h264 only, core projections; good for CI smoke tests
#
# Needs ffmpeg with libx264/libx265/libvpx-vp9 and libsvtav1 or libaom-av1
# (missing encoders are skipped). Writes index.tsv describing every file.
set -euo pipefail

OUT=test-videos
EYE=1024
DUR=6
FPS=30
CODECS="h264 hevc vp9 av1"
QUICK=0
while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT=$2; shift 2 ;;
    --eye) EYE=$2; shift 2 ;;
    --duration) DUR=$2; shift 2 ;;
    --fps) FPS=$2; shift 2 ;;
    --codecs) CODECS=$2; shift 2 ;;
    --quick) QUICK=1; CODECS=h264; shift ;;
    --full) EYE=4096; shift ;;
    -h|--help) sed -n '2,17p' "$0"; exit 0 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done
command -v ffmpeg >/dev/null || { echo "ffmpeg not found" >&2; exit 1; }
FF=(ffmpeg -hide_banner -loglevel error -y)
mkdir -p "$OUT/.work"
WORK=$OUT/.work
ENCODERS=$(ffmpeg -hide_banner -encoders 2>/dev/null)
has_enc() { grep -q " $1 " <<<"$ENCODERS"; }

# drawtext needs a font unless ffmpeg has fontconfig.
FONT=${FONT:-}
for f in /usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf /usr/share/fonts/TTF/DejaVuSans-Bold.ttf \
         /Library/Fonts/Arial.ttf /System/Library/Fonts/Supplemental/Arial.ttf C:/Windows/Fonts/arial.ttf; do
  [ -z "$FONT" ] && [ -f "$f" ] && FONT=$f
done
FONTOPT=${FONT:+fontfile=$FONT:}
txt() { # text x y size [color]
  local t=${1//:/\\:}
  printf "drawtext=%stext='%s':x=%s:y=%s:fontsize=%s:fontcolor=%s:borderw=3:bordercolor=black" \
    "$FONTOPT" "$t" "$2" "$3" "$4" "${5:-white}"
}

# ---------- per-eye still images ----------
# Eye tint: left reddish, right bluish, so swapped eyes are obvious.
tint() { [ "$1" = L ] && echo "0x5a2a2a" || echo "0x2a3a5a"; }

eye_equirect() { # eye fov(180|360) out
  local eye=$1 fov=$2 out=$3 h=$EYE w
  w=$(( fov == 360 ? 2 * EYE : EYE ))
  # Lines every 15 deg (thin) and 45 deg (thick); equator and centre meridian bright.
  local step=$(( w * 15 / fov ))
  local g="geq=lum='if(lt(mod(X,$step),2)+lt(mod(Y,$step),2),200,if(lt(abs(Y-H/2),3)+lt(abs(X-W/2),3),255,lum(X,Y)))':cb='cb(X,Y)':cr='cr(X,Y)'"
  local labels
  if [ "$fov" = 360 ]; then
    labels="$(txt FRONT "w*0.5-tw/2" "h*0.5+20" $((h/16))),$(txt RIGHT "w*0.75-tw/2" "h*0.5+20" $((h/16))),$(txt BACK "20" "h*0.5+20" $((h/16))),$(txt LEFT "w*0.25-tw/2" "h*0.5+20" $((h/16))),$(txt UP "w*0.5-tw/2" "h*0.06" $((h/20))),$(txt DOWN "w*0.5-tw/2" "h*0.9" $((h/20)))"
  else
    labels="$(txt FRONT "w*0.5-tw/2" "h*0.5+20" $((h/14))),$(txt 'LEFT 90' "20" "h*0.5+20" $((h/20))),$(txt 'RIGHT 90' "w-tw-20" "h*0.5+20" $((h/20))),$(txt UP "w*0.5-tw/2" "h*0.06" $((h/20))),$(txt DOWN "w*0.5-tw/2" "h*0.9" $((h/20)))"
  fi
  "${FF[@]}" -f lavfi -i "color=c=$(tint "$eye"):s=${w}x${h}" -frames:v 1 \
    -vf "format=yuv444p,$g,$labels,$(txt "$eye EYE" "w*0.5-tw/2" "h*0.22" $((h/9))),$(txt "EQUIRECT $fov" "w*0.5-tw/2" "h*0.34" $((h/16)))" "$out"
}

eye_fisheye() { # eye fov label out
  local eye=$1 fov=$2 label=$3 out=$4 s=$EYE
  # Rings every 15 deg of field angle (equidistant model), spokes every 30 deg, black outside the circle.
  local g="geq=lum='st(0,hypot(X-W/2,Y-H/2)/(W/2)*$fov/2);st(1,atan2(Y-H/2,X-W/2)*57.2958+180);if(gt(ld(0),$fov/2),0,if(lt(mod(ld(0),15),0.35)+lt(mod(ld(1),30),0.25),220,if(lt(abs(ld(0)-90),0.6),255,lum(X,Y))))':cb='if(gt(hypot(X-W/2,Y-H/2),W/2),128,cb(X,Y))':cr='if(gt(hypot(X-W/2,Y-H/2),W/2),128,cr(X,Y))'"
  "${FF[@]}" -f lavfi -i "color=c=$(tint "$eye"):s=${s}x${s}" -frames:v 1 \
    -vf "format=yuv444p,$g,$(txt "$eye EYE" "w*0.5-tw/2" "h*0.30" $((s/10))),$(txt "$label ${fov}deg" "w*0.5-tw/2" "h*0.62" $((s/16))),$(txt "90deg ring is bright" "w*0.5-tw/2" "h*0.72" $((s/32)))" "$out"
}

eye_flat() { # eye out
  local eye=$1 out=$2 h=$EYE w=$(( EYE * 16 / 9 / 2 * 2 ))
  "${FF[@]}" -f lavfi -i "testsrc2=s=${w}x${h}" -frames:v 1 \
    -vf "drawgrid=w=iw/16:h=ih/9:t=2:c=white@0.5,$(txt "$eye EYE  FLAT 16:9" "w*0.5-tw/2" "h*0.40" $((h/10)))" "$out"
}

eye_eac() { # eye out  (YouTube EAC 3x2: top row L F R, bottom row D B U rotated)
  local eye=$1 out=$2 f=$(( EYE / 2 )) parts=() i=0
  for face in LEFT FRONT RIGHT DOWN BACK UP; do
    "${FF[@]}" -f lavfi -i "color=c=$(tint "$eye"):s=${f}x${f}" -frames:v 1 \
      -vf "drawgrid=w=iw/6:h=ih/6:t=2:c=white@0.6,drawbox=x=0:y=0:w=iw:h=ih:c=yellow:t=4,$(txt "$face" "w*0.5-tw/2" "h*0.45" $((f/7)))" "$WORK/eac$i.png"
    parts+=(-i "$WORK/eac$i.png"); i=$((i+1))
  done
  "${FF[@]}" "${parts[@]}" -frames:v 1 -filter_complex \
    "[0][1][2]hstack=3[top];[3]transpose=1[d];[4]transpose=1[b];[5]transpose=1[u];[d][b][u]hstack=3[bot];[top][bot]vstack,$(txt "$eye EAC" 20 20 $((f/8)))" "$out"
}

make_eye() { # proj eye out
  case "$1" in
    flat) eye_flat "$2" "$3" ;;
    180) eye_equirect "$2" 180 "$3" ;;
    360) eye_equirect "$2" 360 "$3" ;;
    FISHEYE) eye_fisheye "$2" 180 FISHEYE "$3" ;;
    FISHEYE190) eye_fisheye "$2" 190 FISHEYE190 "$3" ;;
    FISHEYE200) eye_fisheye "$2" 200 FISHEYE200 "$3" ;;
    MKX200) eye_fisheye "$2" 200 MKX200 "$3" ;;
    MKX220) eye_fisheye "$2" 220 MKX220 "$3" ;;
    RF52) eye_fisheye "$2" 190 "RF52 dual fisheye" "$3" ;;
    EAC) eye_eac "$2" "$3" ;;
  esac
}

# ---------- frame composition ----------
compose() { # proj stereo out  (stereo: MONO LR RL TB)
  local p=$1 s=$2 out=$3 L=$WORK/$1_L.png R=$WORK/$1_R.png
  [ -f "$L" ] || make_eye "$p" L "$L"
  [ -f "$R" ] || make_eye "$p" R "$R"
  case "$s" in
    MONO) cp "$L" "$out" ;;
    LR) "${FF[@]}" -i "$L" -i "$R" -frames:v 1 -filter_complex "[1]crop=iw-8:ih:0:0,pad=iw+8:ih:8:0[r];[0][r]hstack" "$out" ;;
    RL) "${FF[@]}" -i "$L" -i "$R" -frames:v 1 -filter_complex "[1]crop=iw-8:ih:0:0,pad=iw+8:ih:8:0[r];[r][0]hstack" "$out" ;;
    TB) "${FF[@]}" -i "$L" -i "$R" -frames:v 1 -filter_complex "[1]crop=iw-8:ih:0:0,pad=iw+8:ih:8:0[r];[0][r]vstack" "$out" ;;
  esac
}

# ---------- encoding ----------
enc_args() { # codec -> ffmpeg args; ext on stdout line 1
  case "$1" in
    h264) has_enc libx264 && echo "mp4 -c:v libx264 -preset veryfast -crf 20 -pix_fmt yuv420p -profile:v high" ;;
    hevc) has_enc libx265 && echo "mp4 -c:v libx265 -preset fast -crf 22 -pix_fmt yuv420p10le -tag:v hvc1 -x265-params log-level=error" ;;
    vp9) has_enc libvpx-vp9 && echo "webm -c:v libvpx-vp9 -b:v 0 -crf 34 -row-mt 1 -deadline realtime -cpu-used 8 -pix_fmt yuv420p" ;;
    av1)
      if has_enc libsvtav1; then echo "mkv -c:v libsvtav1 -preset 10 -crf 35 -pix_fmt yuv420p10le"
      elif has_enc libaom-av1; then echo "mkv -c:v libaom-av1 -cpu-used 8 -crf 35 -b:v 0 -row-mt 1 -pix_fmt yuv420p"
      fi ;;
  esac
}

audio_for() { # ext -> audio args
  case "$1" in webm) echo "-c:a libopus -b:a 96k" ;; *) echo "-c:a aac -b:a 128k" ;; esac
}

# Overlay: timecode, frame number and a marker sweeping across each eye once
# per second (judder/dropped-frame check).
motion_vf() {
  echo "$(txt '%{pts:hms}  f=%{n}' "w*0.02" "h*0.92" $((EYE/24)) yellow),drawtext=${FONTOPT}text='||':x='mod(t*w/2,w)':y=h*0.84:fontsize=$((EYE/20)):fontcolor=yellow:box=1:boxcolor=yellow"
}

printf 'file\tdescription\n' > "$OUT/index.tsv"
encode() { # still out codec [extra vf] [note]
  local still=$1 base=$2 codec=$3 extra=${4:-} note=${5:-}
  local spec; spec=$(enc_args "$codec") || true
  [ -n "$spec" ] || { echo "  skip $codec (encoder missing)"; return 0; }
  local ext=${spec%% *} args=${spec#* }
  local out="$OUT/$base.$ext"
  # shellcheck disable=SC2046,SC2086
  "${FF[@]}" -loop 1 -framerate "$FPS" -i "$still" -f lavfi -i "sine=f=440:sample_rate=48000,aformat=channel_layouts=stereo" \
    -t "$DUR" -vf "scale=trunc(iw/2)*2:trunc(ih/2)*2,$(motion_vf)${extra:+,$extra}" $args $(audio_for "$ext") -shortest "$out"
  printf '%s\t%s\n' "$(basename "$out")" "$note" >> "$OUT/index.tsv"
  echo "  $(basename "$out")"
}

if [ "$QUICK" = 1 ]; then
  PROJS="flat 180 360 FISHEYE190 MKX200"
  STEREOS="MONO LR TB"
else
  PROJS="flat 180 360 FISHEYE FISHEYE190 FISHEYE200 MKX200 MKX220 RF52 EAC"
  STEREOS="MONO LR RL TB"
fi

echo "==> projection x stereo x codec matrix in $OUT"
for p in $PROJS; do
  for s in $STEREOS; do
    [ "$p" = EAC ] && [ "$s" != MONO ] && [ "$s" != TB ] && continue
    still="$WORK/${p}_$s.png"
    compose "$p" "$s" "$still"
    for c in $CODECS; do
      # Filename tokens are what fp-core's detector reads. Flat 3D uses the
      # movie-style tokens; VR content uses _180/_360/_FISHEYE… + _LR/_TB.
      case "$p" in
        flat) tok=$([ "$s" = MONO ] && echo "flat" || echo "3D_$([ "$s" = TB ] && echo HOU || echo HSBS)$([ "$s" = RL ] && echo _RL)") ;;
        *) tok="${p}_${s}" ;;
      esac
      encode "$still" "fp_${tok}_${c}_${EYE}p" "$c" "" "$p $s $c"
    done
  done
done

echo "==> container metadata (no filename tokens)"
compose 180 LR "$WORK/meta.png"
if has_enc libx264; then
  "${FF[@]}" -loop 1 -framerate "$FPS" -i "$WORK/meta.png" -t "$DUR" -vf "$(motion_vf)" -c:v libx264 -preset veryfast -pix_fmt yuv420p \
    -metadata:s:v:0 stereo_mode=left_right "$OUT/meta_stereo_mode_tag.mkv"
  printf 'meta_stereo_mode_tag.mkv\tMatroska StereoMode=left_right, no tokens (expect SBS from container)\n' >> "$OUT/index.tsv"
  cp "$OUT/meta_stereo_mode_tag.mkv" "$WORK/plain.mkv"
  "${FF[@]}" -i "$WORK/plain.mkv" -c copy -map_metadata -1 "$OUT/meta_spherical_v1.mp4"
  if command -v spatialmedia >/dev/null || python3 -c 'import spatialmedia' 2>/dev/null; then
    python3 -m spatialmedia -i --stereo=left-right "$OUT/meta_spherical_v1.mp4" "$WORK/inj.mp4" && mv "$WORK/inj.mp4" "$OUT/meta_spherical_v1.mp4"
    printf 'meta_spherical_v1.mp4\tGoogle spatial media v1 (360 equirect, left-right) injected\n' >> "$OUT/index.tsv"
  else
    rm -f "$OUT/meta_spherical_v1.mp4"
    echo "  (install google/spatial-media to also produce meta_spherical_v1.mp4)"
  fi
fi

echo "==> HDR"
if has_enc libx265; then
  compose 180 LR "$WORK/hdr.png"
  # Ramp + bright patches so tone mapping is visible.
  hdrvf="$(motion_vf),drawbox=x=iw*0.1:y=ih*0.1:w=iw*0.1:h=ih*0.1:c=white:t=fill"
  "${FF[@]}" -loop 1 -framerate "$FPS" -i "$WORK/hdr.png" -t "$DUR" -vf "$hdrvf" -c:v libx265 -preset fast -crf 20 \
    -pix_fmt yuv420p10le -tag:v hvc1 -color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc \
    -x265-params "log-level=error:hdr10=1:repeat-headers=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1):max-cll=1000,400" \
    "$OUT/fp_hdr10_180_LR_hevc.mp4"
  "${FF[@]}" -loop 1 -framerate "$FPS" -i "$WORK/hdr.png" -t "$DUR" -vf "$hdrvf" -c:v libx265 -preset fast -crf 20 \
    -pix_fmt yuv420p10le -tag:v hvc1 -color_primaries bt2020 -color_trc arib-std-b67 -colorspace bt2020nc \
    -x265-params "log-level=error:colorprim=bt2020:transfer=arib-std-b67:colormatrix=bt2020nc" "$OUT/fp_hlg_180_LR_hevc.mp4"
  printf 'fp_hdr10_180_LR_hevc.mp4\tHDR10 PQ, mastering metadata\nfp_hlg_180_LR_hevc.mp4\tHLG\n' >> "$OUT/index.tsv"
fi

echo "==> audio layouts"
if has_enc libx264; then
  compose 360 MONO "$WORK/a.png"
  vin=(-loop 1 -framerate "$FPS" -i "$WORK/a.png")
  vout=(-map 0:v -map 1:a -t "$DUR" -vf "$(motion_vf)" -c:v libx264 -preset veryfast -pix_fmt yuv420p)
  # 5.1: each channel a different pitch (FL 300 FR 400 FC 500 LFE 60 BL 600 BR 700 Hz).
  "${FF[@]}" "${vin[@]}" -f lavfi -i "aevalsrc=sin(300*2*PI*t)|sin(400*2*PI*t)|sin(500*2*PI*t)|sin(60*2*PI*t)|sin(600*2*PI*t)|sin(700*2*PI*t):s=48000:c=5.1" \
    "${vout[@]}" -c:a aac -b:a 384k -shortest "$OUT/fp_audio51_360_MONO_h264.mp4"
  # First-order ambiX (ACN W,Y,Z,X; SN3D): a 440 Hz source circling the listener every 8 s.
  "${FF[@]}" "${vin[@]}" -f lavfi -i "aevalsrc=0.5*sin(440*2*PI*t)|0.5*sin(440*2*PI*t)*sin(2*PI*t/8)|0|0.5*sin(440*2*PI*t)*cos(2*PI*t/8):s=48000:c=4.0" \
    "${vout[@]}" -c:a pcm_s16le -shortest -metadata:s:a:0 title=ambiX "$OUT/fp_ambix_foa_360_MONO_h264.mov"
  printf 'fp_audio51_360_MONO_h264.mp4\t5.1 discrete tones\nfp_ambix_foa_360_MONO_h264.mov\tFOA ambiX source orbiting every 8 s\n' >> "$OUT/index.tsv"
fi

echo "==> subtitles"
if has_enc libx264; then
  cat > "$OUT/fp_subs_flat_h264.srt" <<'EOF_SRT'
1
00:00:00,500 --> 00:00:02,500
SRT line one: subtitles at depth

2
00:00:03,000 --> 00:00:05,500
SRT line two with <i>italics</i>
EOF_SRT
  cat > "$OUT/fp_subs_flat_h264.vtt" <<'EOF_VTT'
WEBVTT

00:00:00.500 --> 00:00:02.500
WebVTT cue one

00:00:03.000 --> 00:00:05.500
WebVTT cue two
EOF_VTT
  cat > "$OUT/fp_subs_flat_h264.ass" <<'EOF_ASS'
[Script Info]
ScriptType: v4.00+
PlayResX: 1920
PlayResY: 1080

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,DejaVu Sans,64,&H00FFFFFF,&H000000FF,&H00000000,&H64000000,0,0,0,0,100,100,0,0,1,3,1,2,40,40,60,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:00.50,0:00:02.50,Default,,0,0,0,,{\c&H00FFFF&}ASS styled cue one
Dialogue: 0,0:00:03.00,0:00:05.50,Default,,0,0,0,,{\an8}ASS top-aligned cue two
EOF_ASS
  compose flat MONO "$WORK/s.png"
  "${FF[@]}" -loop 1 -framerate "$FPS" -i "$WORK/s.png" -i "$OUT/fp_subs_flat_h264.srt" -i "$OUT/fp_subs_flat_h264.ass" -t "$DUR" \
    -vf "$(motion_vf)" -map 0:v -map 1 -map 2 -c:v libx264 -preset veryfast -pix_fmt yuv420p -c:s:0 srt -c:s:1 ass \
    -metadata:s:s:0 language=eng -metadata:s:s:1 language=deu "$OUT/fp_subs_flat_h264.mkv"
  printf 'fp_subs_flat_h264.mkv\tembedded SRT (eng) + ASS (deu); sidecar .srt/.vtt/.ass\n' >> "$OUT/index.tsv"
fi

echo "==> frame pacing (flat, 60 / 59.94 / 23.976 fps)"
if has_enc libx264; then
  for r in 60 60000/1001 24000/1001; do
    n=${r//\//_}
    "${FF[@]}" -loop 1 -framerate "$r" -i "$WORK/flat_MONO.png" -t "$DUR" -vf "$(motion_vf)" -r "$r" \
      -c:v libx264 -preset veryfast -pix_fmt yuv420p "$OUT/fp_pacing_${n}fps_flat_h264.mp4" 2>/dev/null || true
    printf 'fp_pacing_%sfps_flat_h264.mp4\tmarker must move smoothly\n' "$n" >> "$OUT/index.tsv"
  done
fi

rm -rf "$WORK"
echo "==> $(($(wc -l < "$OUT/index.tsv") - 1)) files; see $OUT/index.tsv"

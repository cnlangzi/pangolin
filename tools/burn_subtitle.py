#!/usr/bin/env python3
"""
使用MoviePy烧录字幕
更智能的字幕处理方式
"""
import sys
import argparse
from pathlib import Path

def burn_subtitle(movie_path, subtitle_path, output_path, font_size=24, color='white'):
    """使用MoviePy烧录字幕"""
    from moviepy import VideoFileClip, TextClip, CompositeVideoClip
    
    print(f"📹 加载视频: {movie_path}")
    video = VideoFileClip(movie_path)
    
    print(f"📝 加载字幕: {subtitle_path}")
    # 解析字幕文件
    subtitles = parse_subtitle(subtitle_path, video.duration)
    
    print(f"🔧 生成 {len(subtitles)} 条字幕片段...")
    
    # 创建字幕 clips
    text_clips = []
    for sub in subtitles:
        txt_clip = TextClip(
            sub['text'],
            fontsize=font_size,
            color=color,
            font='Arial',
            method='caption',
            size=(video.w - 100, None),  # 自动换行
            stroke_color='black',
            stroke_width=2,
        )
        
        # 设置位置和时长
        txt_clip = txt_clip.with_position(('center', 'bottom'), sub['start'], sub['end'])
        text_clips.append(txt_clip)
    
    # 合成视频
    print("🎬 合成视频...")
    final = CompositeVideoClip([video] + text_clips)
    
    # 输出
    print(f"💾 输出: {output_path}")
    final.write_videofile(
        output_path,
        codec='libx264',
        audio_codec='aac',
        fps=video.fps,
        preset='fast',
        threads=4
    )
    
    print("✅ 完成!")

def parse_subtitle(srt_path, video_duration):
    """解析SRT字幕文件"""
    import re
    
    subtitles = []
    
    with open(srt_path, 'r', encoding='utf-8') as f:
        content = f.read()
    
    # 分割字幕块
    blocks = re.split(r'\n\n+', content)
    
    for block in blocks:
        lines = block.strip().split('\n')
        if len(lines) < 3:
            continue
        
        # 解析时间
        time_line = lines[1]
        times = time_line.split(' --> ')
        if len(times) != 2:
            continue
        
        start = parse_time(times[0].strip())
        end = parse_time(times[1].strip())
        
        # 解析文本
        text = ' '.join(lines[2:])
        
        subtitles.append({
            'start': start,
            'end': end,
            'text': text
        })
    
    return subtitles

def parse_time(time_str):
    """解析SRT时间格式 00:00:00,000 -> 秒"""
    import re
    match = re.match(r'(\d+):(\d+):(\d+)[,\.](\d+)', time_str)
    if not match:
        return 0
    
    h, m, s, ms = match.groups()
    return int(h) * 3600 + int(m) * 60 + int(s) + int(ms) / 1000

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description='烧录字幕')
    parser.add_argument('video', help='视频文件')
    parser.add_argument('subtitle', help='字幕文件(SRT)')
    parser.add_argument('-o', '--output', help='输出文件')
    parser.add_argument('-s', '--size', type=int, default=24, help='字体大小')
    parser.add_argument('-c', '--color', default='white', help='字体颜色')
    
    args = parser.parse_args()
    
    if not args.output:
        args.output = Path(args.video).with_name(
            Path(args.video).stem + '_subtitled.mp4'
        )
    
    burn_subtitle(args.video, args.subtitle, args.output, args.size, args.color)

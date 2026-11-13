import { useMemo } from "react";
import type { SegmentDto } from "./api";

interface Props {
  segments: SegmentDto[];
  total: number | null;
  downloaded: number;
}

// 把连接按速度分成几个色阶，一眼看出谁快谁慢。
function speedColor(speed: number, max: number): string {
  if (max <= 0) return "var(--accent)";
  const ratio = Math.min(1, speed / max);
  // 慢的偏冷，快的偏暖
  const hue = 205 - ratio * 170;
  const light = 42 + ratio * 14;
  return `hsl(${Math.round(hue)} ${Math.round(light + 18)}% ${Math.round(46 + ratio * 8)}%)`;
}

export function SegmentBar({ segments, total, downloaded }: Props) {
  const model = useMemo(() => {
    if (!total || total <= 0 || segments.length === 0) {
      return null;
    }

    // 按起点排序
    const sorted = [...segments].sort((a, b) => a.start - b.start);
    const maxSpeed = Math.max(...segments.map((s) => s.speed), 1);

    const bars = sorted.map((s) => ({
      left: (s.start / total) * 100,
      width: ((s.end - s.start) / total) * 100,
      speed: s.speed,
      color: speedColor(s.speed, maxSpeed)
    }));

    return { maxSpeed, bars };
  }, [segments, total]);

  if (!model) {
    // 没有分段信息时，退化成一条普通的进度条
    const percent = total ? Math.min(100, (downloaded / total) * 100) : 0;
    return (
      <div className="segment-bar plain">
        <div className="segment-fill" style={{ width: `${percent}%` }} />
      </div>
    );
  }

  return (
    <div className="segment-bar" title="各连接持有的区间，颜色深浅代表速度">
      {model.bars.map((bar, i) => (
        <div
          key={i}
          className="segment-slot"
          style={{
            left: `${bar.left}%`,
            width: `${bar.width}%`,
            background: bar.color
          }}
        />
      ))}
    </div>
  );
}

import { useMemo } from "react";
import type { PieceStatusDto } from "./api";

interface Props {
  pieces: PieceStatusDto | null;
}

const STATE_LABEL: Record<string, string> = {
  missing: "待下载",
  partial: "部分下载",
  verified: "已校验",
  corrupted: "校验失败（重传中）"
};

/**
 * BT 分片可视化。
 *
 * 一个 BT 种子由若干 piece 组成，每块有独立的 SHA1。
 * 这里按顺序把分片画成一排小竖条：
 *   - 绿：已经拿到并通过哈希校验
 *   - 浅绿：一块里下了一部分
 *   - 红：校验失败过（坏块），正在重传
 *   - 灰：还没拿到
 *
 * 分片多的时候引擎那边已经降采样过了（每格代表一段分片），
 * 这里直接画。
 */
export function PieceBar({ pieces }: Props) {
  const model = useMemo(() => {
    if (!pieces || pieces.total === 0 || pieces.cells.length === 0) {
      return null;
    }
    return {
      verified: pieces.verified,
      corrupted: pieces.corrupted,
      cells: pieces.cells,
      total: pieces.total
    };
  }, [pieces]);

  if (!model) return null;

  const pending = model.total - model.verified - model.corrupted;

  return (
    <div className="piece-wrap">
      <div className="piece-bar" title={`共 ${model.total} 个分片`}>
        {model.cells.map((cell, i) => (
          <span
            key={i}
            className={`piece-cell ${cell.state}`}
            title={`分片 ${cell.from}–${cell.to} · ${STATE_LABEL[cell.state] ?? cell.state}${
              cell.availability > 0
                ? ` · 最稀缺的一块有 ${cell.availability} 个 peer 持有`
                : ""
            }`}
          />
        ))}
      </div>
      <div className="piece-legend">
        <span className="piece-stat verified">已校验 {model.verified}</span>
        {model.corrupted > 0 && (
          <span className="piece-stat corrupted">坏块 {model.corrupted}</span>
        )}
        {pieces!.verification_failures > 0 && (
          <span className="piece-stat corrupted">
            重传 {pieces!.verification_failures} 次
          </span>
        )}
        <span className="piece-stat missing">待下载 {pending}</span>
      </div>
    </div>
  );
}

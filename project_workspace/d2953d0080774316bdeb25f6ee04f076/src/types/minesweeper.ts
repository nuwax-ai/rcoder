/**
 * 扫雷游戏的核心类型定义
 */

// 单元格状态
export enum CellState {
  HIDDEN = 'hidden',     // 未翻开
  REVEALED = 'revealed', // 已翻开
  FLAGGED = 'flagged',   // 已标记
}

// 单元格数据
export interface Cell {
  row: number;
  col: number;
  isMine: boolean;       // 是否是地雷
  state: CellState;      // 单元格状态
  adjacentMines: number; // 周围地雷数量
}

// 游戏难度
export enum Difficulty {
  BEGINNER = 'beginner',   // 初级: 9x9, 10个雷
  INTERMEDIATE = 'intermediate', // 中级: 16x16, 40个雷
  EXPERT = 'expert',       // 高级: 16x30, 99个雷
  CUSTOM = 'custom',       // 自定义
}

// 游戏配置
export interface GameConfig {
  rows: number;
  cols: number;
  mines: number;
  difficulty: Difficulty;
}

// 游戏状态
export enum GameState {
  READY = 'ready',       // 准备开始
  PLAYING = 'playing',   // 游戏中
  WON = 'won',          // 游戏胜利
  LOST = 'lost',        // 游戏失败
  PAUSED = 'paused',     // 游戏暂停
}

// 游戏统计
export interface GameStats {
  time: number;          // 游戏时间（秒）
  flagsLeft: number;     // 剩余旗帜数
  cellsRevealed: number; // 已翻开的格子数
  totalCells: number;    // 总格子数
}

// 游戏数据
export interface GameData {
  board: Cell[][];       // 游戏板
  config: GameConfig;    // 游戏配置
  state: GameState;      // 游戏状态
  stats: GameStats;      // 游戏统计
  startTime: number | null; // 开始时间
  endTime: number | null;   // 结束时间
}

// 预定义的游戏配置
export const DEFAULT_CONFIGS: Record<Difficulty, Omit<GameConfig, 'difficulty'>> = {
  [Difficulty.BEGINNER]: {
    rows: 9,
    cols: 9,
    mines: 10,
  },
  [Difficulty.INTERMEDIATE]: {
    rows: 16,
    cols: 16,
    mines: 40,
  },
  [Difficulty.EXPERT]: {
    rows: 16,
    cols: 30,
    mines: 99,
  },
  [Difficulty.CUSTOM]: {
    rows: 16,
    cols: 16,
    mines: 40,
  },
};
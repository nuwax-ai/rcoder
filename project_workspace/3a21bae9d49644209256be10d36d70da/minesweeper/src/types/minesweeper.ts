// 扫雷游戏的核心类型定义

export type CellState = 'hidden' | 'revealed' | 'flagged' | 'questioned';

export type GameState = 'idle' | 'playing' | 'won' | 'lost';

export type Difficulty = 'beginner' | 'intermediate' | 'expert' | 'custom';

export interface Cell {
  row: number;
  col: number;
  isMine: boolean;
  adjacentMines: number;
  state: CellState;
}

export interface GameConfig {
  rows: number;
  cols: number;
  mines: number;
}

export interface GameStats {
  timeElapsed: number;
  flagsPlaced: number;
  cellsRevealed: number;
  totalCells: number;
}

export interface MinesweeperGame {
  config: GameConfig;
  board: Cell[][];
  gameState: GameState;
  stats: GameStats;
  difficulty: Difficulty;
}

// 预设的游戏难度配置
export const DIFFICULTY_CONFIGS: Record<Difficulty, GameConfig> = {
  beginner: { rows: 9, cols: 9, mines: 10 },
  intermediate: { rows: 16, cols: 16, mines: 40 },
  expert: { rows: 16, cols: 30, mines: 99 },
  custom: { rows: 16, cols: 16, mines: 40 }
};
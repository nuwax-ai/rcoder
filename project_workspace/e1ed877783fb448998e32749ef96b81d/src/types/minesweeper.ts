/**
 * 扫雷游戏的类型定义
 */

export enum CellState {
  HIDDEN = 'hidden',
  REVEALED = 'revealed',
  FLAGGED = 'flagged',
}

export enum GameState {
  READY = 'ready',
  PLAYING = 'playing',
  WON = 'won',
  LOST = 'lost',
}

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
  flagsUsed: number;
  cellsRevealed: number;
}

export const DIFFICULTY_LEVELS: Record<string, GameConfig> = {
  beginner: { rows: 9, cols: 9, mines: 10 },
  intermediate: { rows: 16, cols: 16, mines: 40 },
  expert: { rows: 16, cols: 30, mines: 99 },
};
'use client';

import { useState, useCallback, useEffect } from 'react';
import { Cell, GameBoard, GameState, GameConfig } from '@/types/minesweeper';
import {
  initializeBoard,
  revealCell,
  toggleFlag,
  checkWin,
  revealAllMines,
  calculateGameStats,
} from '@/utils/minesweeper';
import { GameBoard as GameBoardComponent } from './GameBoard';
import { GameControls } from './GameControls';
import { DifficultySelector } from './DifficultySelector';

const DIFFICULTIES: Record<string, GameConfig> = {
  beginner: { rows: 9, cols: 9, mines: 10 },
  intermediate: { rows: 16, cols: 16, mines: 40 },
  expert: { rows: 16, cols: 30, mines: 99 },
  custom: { rows: 10, cols: 10, mines: 15 },
};

export function MinesweeperGame() {
  const [difficulty, setDifficulty] = useState<string>('beginner');
  const [gameConfig, setGameConfig] = useState<GameConfig>(DIFFICULTIES.beginner);
  const [board, setBoard] = useState<Cell[][]>([]);
  const [gameState, setGameState] = useState<GameState>('idle');
  const [isFirstClick, setIsFirstClick] = useState(true);
  const [gameStats, setGameStats] = useState({
    revealedCells: 0,
    flaggedCells: 0,
    remainingMines: 0,
  });

  // 初始化游戏板
  const initializeGame = useCallback(() => {
    const newBoard = Array.from({ length: gameConfig.rows }, () =>
      Array.from({ length: gameConfig.cols }, () => ({
        isMine: false,
        state: 'hidden' as const,
        adjacentMines: 0,
      }))
    );
    setBoard(newBoard);
    setGameState('idle');
    setIsFirstClick(true);
    setGameStats({
      revealedCells: 0,
      flaggedCells: 0,
      remainingMines: gameConfig.mines,
    });
  }, [gameConfig]);

  // 处理难度变化
  const handleDifficultyChange = useCallback((newDifficulty: string) => {
    setDifficulty(newDifficulty);
    const newConfig = DIFFICULTIES[newDifficulty];
    setGameConfig(newConfig);
  }, []);

  // 处理格子点击
  const handleCellClick = useCallback(
    (row: number, col: number) => {
      if (gameState === 'won' || gameState === 'lost') {
        return;
      }

      let newBoard = [...board.map(row => [...row])];

      // 第一次点击时初始化地雷位置
      if (isFirstClick) {
        newBoard = initializeBoard(gameConfig, row, col);
        setIsFirstClick(false);
        setGameState('playing');
      }

      // 揭示格子
      const hitMine = revealCell(newBoard, row, col);
      setBoard(newBoard);

      // 检查游戏结果
      if (hitMine) {
        revealAllMines(newBoard);
        setBoard(newBoard);
        setGameState('lost');
      } else if (checkWin(newBoard)) {
        setGameState('won');
      }

      // 更新统计信息
      const stats = calculateGameStats(newBoard);
      setGameStats(stats);
    },
    [board, gameState, isFirstClick, gameConfig]
  );

  // 处理右键点击（插旗）
  const handleCellRightClick = useCallback(
    (row: number, col: number) => {
      if (gameState === 'won' || gameState === 'lost' || isFirstClick) {
        return;
      }

      const newBoard = [...board.map(row => [...row])];
      toggleFlag(newBoard, row, col);
      setBoard(newBoard);

      // 更新统计信息
      const stats = calculateGameStats(newBoard);
      setGameStats(stats);
    },
    [board, gameState, isFirstClick]
  );

  // 处理重新开始
  const handleRestart = useCallback(() => {
    initializeGame();
  }, [initializeGame]);

  // 初始化游戏
  useEffect(() => {
    initializeGame();
  }, [initializeGame]);

  const totalCells = gameConfig.rows * gameConfig.cols;

  return (
    <div className="min-h-screen bg-gradient-to-br from-blue-50 to-indigo-100 p-4">
      <div className="max-w-7xl mx-auto">
        <div className="text-center mb-8">
          <h1 className="text-4xl font-bold text-gray-900 mb-2">扫雷游戏</h1>
          <p className="text-gray-600">经典扫雷游戏，找出所有地雷！</p>
        </div>

        <div className="grid grid-cols-1 lg:grid-cols-4 gap-6">
          {/* 左侧控制面板 */}
          <div className="lg:col-span-1 space-y-6">
            <DifficultySelector
              currentDifficulty={difficulty}
              onDifficultyChange={handleDifficultyChange}
            />
            <GameControls
              gameState={gameState}
              remainingMines={gameStats.remainingMines}
              revealedCells={gameStats.revealedCells}
              totalCells={totalCells}
              onRestart={handleRestart}
            />
          </div>

          {/* 右侧游戏板 */}
          <div className="lg:col-span-3 flex justify-center">
            <div className="bg-white rounded-lg shadow-lg p-6">
              <GameBoardComponent
                cells={board}
                onCellClick={handleCellClick}
                onCellRightClick={handleCellRightClick}
                cellSize={
                  gameConfig.cols > 20 ? 'small' :
                  gameConfig.cols > 15 ? 'medium' : 'large'
                }
              />
            </div>
          </div>
        </div>

        {/* 游戏说明 */}
        <div className="mt-8 text-center text-sm text-gray-600">
          <p>左键点击揭开格子 • 右键点击插旗标记地雷 • 数字表示周围8个格子中的地雷数量</p>
        </div>
      </div>
    </div>
  );
}
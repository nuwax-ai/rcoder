/**
 * 扫雷游戏主组件
 */

import React, { useState, useEffect, useCallback, useRef } from 'react';
import { GameData, GameState, GameConfig, Difficulty, CellState } from '../../types/minesweeper';
import {
  createNewGame,
  startGame,
  revealCell,
  toggleFlag,
  checkWin,
  revealAllMines,
  formatTime,
  getGameConfig
} from '../../lib/minesweeper-utils';
import GameHeader from './GameHeader';
import GameBoard from './GameBoard';
import DifficultySelector from './DifficultySelector';

const MinesweeperGame: React.FC = () => {
  const [gameData, setGameData] = useState<GameData>(() => {
    const config = getGameConfig(Difficulty.BEGINNER);
    return createNewGame(config);
  });
  const [showDifficultySelector, setShowDifficultySelector] = useState(false);
  const timerRef = useRef<NodeJS.Timeout | null>(null);

  // 游戏计时器
  useEffect(() => {
    if (gameData.state === GameState.PLAYING && gameData.startTime) {
      timerRef.current = setInterval(() => {
        setGameData(prev => ({
          ...prev,
          stats: {
            ...prev.stats,
            time: Math.floor((Date.now() - (prev.startTime || Date.now())) / 1000)
          }
        }));
      }, 1000);
    } else {
      if (timerRef.current) {
        clearInterval(timerRef.current);
        timerRef.current = null;
      }
    }

    return () => {
      if (timerRef.current) {
        clearInterval(timerRef.current);
      }
    };
  }, [gameData.state, gameData.startTime]);

  // 处理单元格点击
  const handleCellClick = useCallback((row: number, col: number) => {
    setGameData(prevData => {
      let newData = { ...prevData };

      // 如果是第一次点击，开始游戏
      if (prevData.state === GameState.READY) {
        newData = startGame(prevData, row, col);
      }

      // 如果游戏已结束，不处理点击
      if (newData.state !== GameState.PLAYING) {
        return newData;
      }

      // 翻开单元格
      newData = revealCell(newData, row, col);

      // 检查是否踩到地雷
      if (newData.board[row][col].isMine) {
        newData = revealAllMines(newData);
        newData.state = GameState.LOST;
        newData.endTime = Date.now();
        return newData;
      }

      // 检查是否获胜
      if (checkWin(newData)) {
        newData.state = GameState.WON;
        newData.endTime = Date.now();
      }

      return newData;
    });
  }, []);

  // 处理右键点击（标记旗帜）
  const handleCellRightClick = useCallback((row: number, col: number) => {
    setGameData(prevData => {
      // 只在游戏中或准备状态下允许标记
      if (prevData.state === GameState.PLAYING || prevData.state === GameState.READY) {
        return toggleFlag(prevData, row, col);
      }
      return prevData;
    });
  }, []);

  // 重新开始游戏
  const handleRestart = useCallback(() => {
    setGameData(prevData => createNewGame(prevData.config));
  }, []);

  // 新游戏（显示难度选择）
  const handleNewGame = useCallback(() => {
    setShowDifficultySelector(true);
  }, []);

  // 选择难度
  const handleDifficultyChange = useCallback((config: GameConfig) => {
    setGameData(createNewGame(config));
    setShowDifficultySelector(false);
  }, []);

  // 双击处理（快速翻开周围格子）
  const handleCellDoubleClick = useCallback((row: number, col: number) => {
    setGameData(prevData => {
      if (prevData.state !== GameState.PLAYING) {
        return prevData;
      }

      const cell = prevData.board[row][col];

      // 只有已翻开的数字格才能双击
      if (cell.state !== CellState.REVEALED || cell.adjacentMines === 0) {
        return prevData;
      }

      // 计算周围旗帜数量
      let flagCount = 0;
      const neighbors: [number, number][] = [];

      for (let dr = -1; dr <= 1; dr++) {
        for (let dc = -1; dc <= 1; dc++) {
          if (dr === 0 && dc === 0) continue;

          const newRow = row + dr;
          const newCol = col + dc;

          if (
            newRow >= 0 && newRow < prevData.config.rows &&
            newCol >= 0 && newCol < prevData.config.cols
          ) {
            neighbors.push([newRow, newCol]);
            if (prevData.board[newRow][newCol].state === CellState.FLAGGED) {
              flagCount++;
            }
          }
        }
      }

      // 如果旗帜数量等于周围地雷数量，翻开所有未标记的格子
      if (flagCount === cell.adjacentMines) {
        let newData = { ...prevData };

        for (const [nRow, nCol] of neighbors) {
          const neighborCell = newData.board[nRow][nCol];
          if (neighborCell.state === CellState.HIDDEN) {
            newData = revealCell(newData, nRow, nCol);

            // 如果踩到地雷，游戏结束
            if (newData.board[nRow][nCol].isMine) {
              newData = revealAllMines(newData);
              newData.state = GameState.LOST;
              newData.endTime = Date.now();
              return newData;
            }
          }
        }

        // 检查是否获胜
        if (checkWin(newData)) {
          newData.state = GameState.WON;
          newData.endTime = Date.now();
        }

        return newData;
      }

      return prevData;
    });
  }, []);

  // 键盘快捷键
  useEffect(() => {
    const handleKeyPress = (e: KeyboardEvent) => {
      switch (e.key.toLowerCase()) {
        case 'r':
          handleRestart();
          break;
        case 'n':
          handleNewGame();
          break;
        case 'escape':
          setShowDifficultySelector(false);
          break;
      }
    };

    window.addEventListener('keydown', handleKeyPress);
    return () => window.removeEventListener('keydown', handleKeyPress);
  }, [handleRestart, handleNewGame]);

  if (showDifficultySelector) {
    return (
      <div className="min-h-screen bg-gradient-to-br from-blue-50 to-indigo-100 flex items-center justify-center p-4">
        <DifficultySelector
          currentDifficulty={gameData.config.difficulty}
          onDifficultyChange={handleDifficultyChange}
        />
      </div>
    );
  }

  return (
    <div className="min-h-screen bg-gradient-to-br from-blue-50 to-indigo-100 flex flex-col items-center justify-center p-4">
      <div className="bg-white rounded-lg shadow-2xl overflow-hidden">
        <GameHeader
          gameData={gameData}
          onRestart={handleRestart}
          onNewGame={handleNewGame}
        />
        <div className="p-4 bg-gray-100">
          <GameBoard
            gameData={gameData}
            onCellClick={handleCellClick}
            onCellRightClick={handleCellRightClick}
          />
        </div>
      </div>

      {/* 游戏说明 */}
      <div className="mt-6 text-center text-gray-600 max-w-md">
        <h3 className="font-semibold mb-2">游戏说明</h3>
        <div className="text-sm space-y-1">
          <p>左键点击翻开格子，右键标记地雷</p>
          <p>数字表示周围8个格子中的地雷数量</p>
          <p>标记出所有地雷或翻开所有安全格子即可获胜</p>
          <p>按 R 重新开始，按 N 选择新难度</p>
        </div>
      </div>

      {/* 游戏状态弹窗 */}
      {(gameData.state === GameState.WON || gameData.state === GameState.LOST) && (
        <div className="fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50">
          <div className="bg-white rounded-lg p-8 text-center max-w-sm mx-4">
            <div className="text-6xl mb-4">
              {gameData.state === GameState.WON ? '🎉' : '💥'}
            </div>
            <h2 className="text-2xl font-bold mb-2">
              {gameData.state === GameState.WON ? '恭喜你赢了！' : '游戏结束！'}
            </h2>
            <p className="text-gray-600 mb-4">
              用时：{formatTime(gameData.stats.time)}
            </p>
            <div className="space-y-2">
              <button
                onClick={handleRestart}
                className="w-full bg-blue-500 text-white px-6 py-2 rounded-lg hover:bg-blue-600 transition-colors duration-150"
              >
                再玩一次
              </button>
              <button
                onClick={handleNewGame}
                className="w-full bg-gray-300 text-gray-700 px-6 py-2 rounded-lg hover:bg-gray-400 transition-colors duration-150"
              >
                选择难度
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
};

export default MinesweeperGame;
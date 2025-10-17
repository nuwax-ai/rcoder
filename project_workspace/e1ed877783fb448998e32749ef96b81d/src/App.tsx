import React, { useState } from 'react';
import './App.css';
import { useMinesweeper } from './hooks/useMinesweeper';
import { useKeyboardShortcuts } from './hooks/useKeyboardShortcuts';
import { GameBoard } from './components/GameBoard';
import { GameInfo } from './components/GameInfo';
import { DifficultySelector } from './components/DifficultySelector';
import { GameControls } from './components/GameControls';
import { DIFFICULTY_LEVELS } from './types/minesweeper';
import { GameState } from './types/minesweeper';

function App() {
  const [currentDifficulty, setCurrentDifficulty] = useState<string>('beginner');
  const [gameConfig, setGameConfig] = useState(DIFFICULTY_LEVELS.beginner);

  const {
    board,
    gameState,
    gameStats,
    handleCellClick,
    handleRightClick,
    handleDoubleClick,
    startNewGame,
  } = useMinesweeper(gameConfig);

  const handleDifficultyChange = (difficulty: string, config: typeof DIFFICULTY_LEVELS.beginner) => {
    setCurrentDifficulty(difficulty);
    setGameConfig(config);
  };

  const handleNewGame = () => {
    startNewGame();
  };

  // 键盘快捷键
  useKeyboardShortcuts({
    onNewGame: handleNewGame,
    onDifficultyChange: handleDifficultyChange,
  });

  return (
    <div className="min-h-screen bg-gradient-to-br from-blue-50 to-indigo-100 py-8">
      <div className="container mx-auto px-4 max-w-4xl">
        {/* 游戏标题 */}
        <header className="text-center mb-8">
          <h1 className="text-4xl font-bold text-gray-800 mb-2">
            💣 扫雷游戏
          </h1>
          <p className="text-gray-600">
            经典的扫雷游戏，使用 React + TypeScript 构建
          </p>
        </header>

        {/* 难度选择 */}
        <DifficultySelector
          currentDifficulty={currentDifficulty}
          onDifficultyChange={handleDifficultyChange}
        />

        {/* 游戏信息面板 */}
        <GameInfo
          gameState={gameState}
          gameStats={gameStats}
          config={gameConfig}
          onNewGame={handleNewGame}
        />

        {/* 游戏板 */}
        <div className="flex justify-center mb-6">
          <GameBoard
            board={board}
            onCellClick={handleCellClick}
            onRightClick={handleRightClick}
            onDoubleClick={handleDoubleClick}
            isGameLost={gameState === GameState.LOST}
          />
        </div>

        {/* 游戏控制说明 */}
        <GameControls />

        {/* 页脚 */}
        <footer className="text-center mt-12 text-sm text-gray-500">
          <p>
            使用 React 18 + Vite + TypeScript + Tailwind CSS 构建
          </p>
          <p className="mt-1">
            支持键盘快捷键：R - 新游戏 | 1-3 - 选择难度
          </p>
        </footer>
      </div>
    </div>
  );
}

export default App;
import React from 'react';
import { GameState, GameConfig } from '../types/minesweeper';

interface GameInfoProps {
  gameState: GameState;
  gameStats: {
    timeElapsed: number;
    flagsUsed: number;
    cellsRevealed: number;
  };
  config: GameConfig;
  onNewGame: () => void;
}

export const GameInfo: React.FC<GameInfoProps> = ({
  gameState,
  gameStats,
  config,
  onNewGame,
}) => {
  const formatTime = (seconds: number): string => {
    const mins = Math.floor(seconds / 60);
    const secs = seconds % 60;
    return `${mins.toString().padStart(2, '0')}:${secs.toString().padStart(2, '0')}`;
  };

  const getGameStateEmoji = (): string => {
    switch (gameState) {
      case GameState.READY:
        return '😊';
      case GameState.PLAYING:
        return '😊';
      case GameState.WON:
        return '😎';
      case GameState.LOST:
        return '😵';
      default:
        return '😊';
    }
  };

  const getGameStateMessage = (): string => {
    switch (gameState) {
      case GameState.READY:
        return '点击任意格子开始游戏';
      case GameState.PLAYING:
        return '游戏进行中...';
      case GameState.WON:
        return '恭喜你赢了！🎉';
      case GameState.LOST:
        return '游戏结束！💥';
      default:
        return '';
    }
  };

  const getGameStateColor = (): string => {
    switch (gameState) {
      case GameState.WON:
        return 'text-green-600';
      case GameState.LOST:
        return 'text-red-600';
      default:
        return 'text-gray-700';
    }
  };

  const remainingMines = config.mines - gameStats.flagsUsed;

  return (
    <div className="mb-6">
      {/* 游戏状态和控制 */}
      <div className="flex items-center justify-center gap-8 mb-4">
        {/* 地雷计数器 */}
        <div className="flex items-center gap-2 bg-gray-800 text-red-500 px-4 py-2 rounded font-mono text-lg font-bold">
          <span>💣</span>
          <span>{remainingMines.toString().padStart(3, '0')}</span>
        </div>

        {/* 新游戏按钮 */}
        <button
          onClick={onNewGame}
          className="text-4xl hover:scale-110 transition-transform duration-200 focus:outline-none focus:ring-2 focus:ring-blue-400 rounded"
          title="新游戏"
        >
          {getGameStateEmoji()}
        </button>

        {/* 计时器 */}
        <div className="flex items-center gap-2 bg-gray-800 text-green-400 px-4 py-2 rounded font-mono text-lg font-bold">
          <span>⏱️</span>
          <span>{formatTime(gameStats.timeElapsed)}</span>
        </div>
      </div>

      {/* 游戏状态消息 */}
      <div className="text-center">
        <p className={`text-lg font-semibold ${getGameStateColor()}`}>
          {getGameStateMessage()}
        </p>
      </div>

      {/* 游戏统计信息 */}
      <div className="flex justify-center gap-6 mt-4 text-sm text-gray-600">
        <div className="text-center">
          <p className="font-semibold">难度</p>
          <p>{config.rows}×{config.cols}</p>
          <p>{config.mines} 个地雷</p>
        </div>
        <div className="text-center">
          <p className="font-semibold">进度</p>
          <p>{gameStats.cellsRevealed} / {config.rows * config.cols - config.mines}</p>
          <p>{Math.round((gameStats.cellsRevealed / (config.rows * config.cols - config.mines)) * 100)}%</p>
        </div>
        <div className="text-center">
          <p className="font-semibold">标记</p>
          <p>{gameStats.flagsUsed} / {config.mines}</p>
        </div>
      </div>
    </div>
  );
};
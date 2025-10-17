/**
 * 扫雷游戏头部信息组件
 */

import React, { memo } from 'react';
import { GameData, GameState, formatTime } from '../../lib/minesweeper-utils';

interface GameHeaderProps {
  gameData: GameData;
  onRestart: () => void;
  onNewGame: () => void;
}

const GameHeader: React.FC<GameHeaderProps> = memo(({
  gameData,
  onRestart,
  onNewGame
}) => {
  const { stats, state } = gameData;

  const getGameStateEmoji = () => {
    switch (state) {
      case GameState.READY:
        return '🙂';
      case GameState.PLAYING:
        return '😮';
      case GameState.WON:
        return '😎';
      case GameState.LOST:
        return '😵';
      case GameState.PAUSED:
        return '😴';
      default:
        return '🙂';
    }
  };

  const getGameStateText = () => {
    switch (state) {
      case GameState.READY:
        return '准备开始';
      case GameState.PLAYING:
        return '游戏中';
      case GameState.WON:
        return '你赢了！';
      case GameState.LOST:
        return '游戏结束';
      case GameState.PAUSED:
        return '游戏暂停';
      default:
        return '';
    }
  };

  return (
    <div className="bg-gray-700 text-white p-4 rounded-t-lg">
      <div className="flex items-center justify-between max-w-2xl mx-auto">
        {/* 地雷计数器 */}
        <div className="flex items-center space-x-2 bg-black text-red-500 px-3 py-1 rounded font-mono text-lg">
          <span>🚩</span>
          <span>{stats.flagsLeft.toString().padStart(3, '0')}</span>
        </div>

        {/* 游戏状态和控制 */}
        <div className="flex flex-col items-center space-y-2">
          <button
            onClick={state === GameState.WON || state === GameState.LOST ? onNewGame : onRestart}
            className="text-4xl hover:scale-110 transition-transform duration-150"
            title={state === GameState.WON || state === GameState.LOST ? '新游戏' : '重新开始'}
          >
            {getGameStateEmoji()}
          </button>
          <div className="text-sm font-medium">
            {getGameStateText()}
          </div>
        </div>

        {/* 计时器 */}
        <div className="flex items-center space-x-2 bg-black text-red-500 px-3 py-1 rounded font-mono text-lg">
          <span>⏱️</span>
          <span>{formatTime(stats.time)}</span>
        </div>
      </div>
    </div>
  );
});

GameHeader.displayName = 'GameHeader';

export default GameHeader;
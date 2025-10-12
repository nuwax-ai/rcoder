'use client';

import React, { useState, useEffect, useCallback, useRef } from 'react';
import { MinesweeperEngine } from '@/lib/minesweeper';
import { MinesweeperGame, Difficulty } from '@/types/minesweeper';
import { MinesweeperGameBoard } from './GameBoard';
import { MinesweeperControlPanel } from './ControlPanel';
import { Card } from '@/components/ui/card';

export const MinesweeperGameComponent: React.FC = () => {
  const [game, setGame] = useState<MinesweeperGame>(() => {
    const engine = new MinesweeperEngine();
    return engine.getGame();
  });

  const [engine] = useState(() => new MinesweeperEngine());
  const [timeElapsed, setTimeElapsed] = useState(0);
  const intervalRef = useRef<NodeJS.Timeout | null>(null);

  // 计时器逻辑
  useEffect(() => {
    if (game.gameState === 'playing') {
      intervalRef.current = setInterval(() => {
        setTimeElapsed((prev) => prev + 1);
      }, 1000);
    } else {
      if (intervalRef.current) {
        clearInterval(intervalRef.current);
        intervalRef.current = null;
      }
    }

    return () => {
      if (intervalRef.current) {
        clearInterval(intervalRef.current);
      }
    };
  }, [game.gameState]);

  // 更新引擎中的时间
  useEffect(() => {
    engine.updateTime(timeElapsed);
  }, [timeElapsed, engine]);

  const handleCellClick = useCallback((row: number, col: number) => {
    const updatedGame = engine.revealCell(row, col);
    setGame(updatedGame);

    // 如果游戏开始，重置计时器
    if (engine.getGame().gameState === 'playing' && timeElapsed === 0) {
      setTimeElapsed(0);
    }
  }, [engine, timeElapsed]);

  const handleCellRightClick = useCallback((row: number, col: number) => {
    const updatedGame = engine.toggleFlag(row, col);
    setGame(updatedGame);
  }, [engine]);

  const handleRestart = useCallback(() => {
    const newGame = engine.restart();
    setGame(newGame);
    setTimeElapsed(0);
  }, [engine]);

  const handleDifficultyChange = useCallback((difficulty: Difficulty) => {
    const newGame = engine.changeDifficulty(difficulty);
    setGame(newGame);
    setTimeElapsed(0);
  }, [engine]);

  return (
    <div className="min-h-screen bg-gradient-to-br from-blue-50 to-indigo-100 py-8">
      <div className="max-w-6xl mx-auto px-4">
        {/* 游戏标题 */}
        <div className="text-center mb-8">
          <h1 className="text-4xl font-bold text-gray-900 mb-2">
            💣 扫雷游戏
          </h1>
          <p className="text-gray-600">
            经典的扫雷游戏，找出所有地雷并标记它们！
          </p>
        </div>

        {/* 游戏控制面板 */}
        <MinesweeperControlPanel
          gameState={game.gameState}
          remainingMines={engine.getRemainingMines()}
          timeElapsed={timeElapsed}
          difficulty={game.difficulty}
          onRestart={handleRestart}
          onDifficultyChange={handleDifficultyChange}
        />

        {/* 游戏板 */}
        <div className="flex justify-center">
          <MinesweeperGameBoard
            game={game}
            onCellClick={handleCellClick}
            onCellRightClick={handleCellRightClick}
          />
        </div>

        {/* 游戏结束提示 */}
        {game.gameState === 'won' && (
          <Card className="mt-6 p-6 bg-green-50 border-green-200 text-center">
            <h2 className="text-2xl font-bold text-green-800 mb-2">
              🎉 恭喜你赢了！
            </h2>
            <p className="text-green-600 mb-4">
              你成功找出了所有 {game.config.mines} 个地雷！
            </p>
            <p className="text-sm text-gray-600">
              用时：{Math.floor(timeElapsed / 60)}分{timeElapsed % 60}秒
            </p>
          </Card>
        )}

        {game.gameState === 'lost' && (
          <Card className="mt-6 p-6 bg-red-50 border-red-200 text-center">
            <h2 className="text-2xl font-bold text-red-800 mb-2">
              💥 游戏结束
            </h2>
            <p className="text-red-600 mb-4">
              很遗憾，你踩到了地雷！
            </p>
            <p className="text-sm text-gray-600">
              再试一次吧！
            </p>
          </Card>
        )}

        {/* 游戏说明 */}
        <Card className="mt-8 p-6 bg-gray-50">
          <h3 className="text-lg font-semibold text-gray-900 mb-3">
            游戏规则
          </h3>
          <div className="grid md:grid-cols-2 gap-4 text-sm text-gray-600">
            <div>
              <h4 className="font-medium text-gray-800 mb-1">基本玩法：</h4>
              <ul className="space-y-1 list-disc list-inside">
                <li>左键点击揭开格子</li>
                <li>右键循环切换：旗帜 → 问号 → 空白</li>
                <li>数字表示周围8个格子中的地雷数量</li>
                <li>标记所有地雷并揭开所有安全格子即获胜</li>
              </ul>
            </div>
            <div>
              <h4 className="font-medium text-gray-800 mb-1">技巧提示：</h4>
              <ul className="space-y-1 list-disc list-inside">
                <li>从角落和边缘开始通常更安全</li>
                <li>如果数字周围的旗帜数量等于该数字，其余格子都是安全的</li>
                <li>如果数字周围的未知格子数等于(数字 - 已标记旗帜)，这些格子都是地雷</li>
                <li>问号标记帮助你记住不确定的位置</li>
              </ul>
            </div>
          </div>
        </Card>
      </div>
    </div>
  );
};
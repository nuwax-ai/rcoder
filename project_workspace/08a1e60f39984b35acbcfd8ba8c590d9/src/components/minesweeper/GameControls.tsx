'use client';

import { GameState } from '@/types/minesweeper';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { RotateCcw, Flag, Trophy, Skull } from 'lucide-react';

interface GameControlsProps {
  gameState: GameState;
  remainingMines: number;
  revealedCells: number;
  totalCells: number;
  onRestart: () => void;
}

export function GameControls({
  gameState,
  remainingMines,
  revealedCells,
  totalCells,
  onRestart,
}: GameControlsProps) {
  const getGameStateDisplay = () => {
    switch (gameState) {
      case 'idle':
        return {
          text: '准备开始',
          icon: null,
          color: 'bg-gray-100 text-gray-800',
        };
      case 'playing':
        return {
          text: '游戏进行中',
          icon: null,
          color: 'bg-blue-100 text-blue-800',
        };
      case 'won':
        return {
          text: '你赢了！',
          icon: <Trophy className="w-4 h-4" />,
          color: 'bg-green-100 text-green-800',
        };
      case 'lost':
        return {
          text: '游戏结束',
          icon: <Skull className="w-4 h-4" />,
          color: 'bg-red-100 text-red-800',
        };
    }
  };

  const gameStateDisplay = getGameStateDisplay();
  const progress = Math.round((revealedCells / totalCells) * 100);

  return (
    <Card className="p-6">
      <div className="flex items-center justify-between mb-4">
        <div className="flex items-center gap-2">
          <Badge className={gameStateDisplay.color}>
            <div className="flex items-center gap-1">
              {gameStateDisplay.icon}
              {gameStateDisplay.text}
            </div>
          </Badge>
        </div>
        <Button onClick={onRestart} variant="outline" size="sm">
          <RotateCcw className="w-4 h-4 mr-2" />
          重新开始
        </Button>
      </div>

      <div className="grid grid-cols-3 gap-4">
        <div className="text-center">
          <div className="flex items-center justify-center gap-2 text-2xl font-bold text-red-600">
            <Flag className="w-5 h-5" />
            {remainingMines}
          </div>
          <div className="text-sm text-gray-600 mt-1">剩余地雷</div>
        </div>

        <div className="text-center">
          <div className="text-2xl font-bold text-blue-600">
            {revealedCells}/{totalCells - (gameState === 'won' || gameState === 'lost' ? remainingMines : 0)}
          </div>
          <div className="text-sm text-gray-600 mt-1">已揭开</div>
        </div>

        <div className="text-center">
          <div className="text-2xl font-bold text-green-600">
            {progress}%
          </div>
          <div className="text-sm text-gray-600 mt-1">进度</div>
        </div>
      </div>

      {gameState === 'playing' && (
        <div className="mt-4">
          <div className="w-full bg-gray-200 rounded-full h-2">
            <div
              className="bg-blue-600 h-2 rounded-full transition-all duration-300"
              style={{ width: `${progress}%` }}
            />
          </div>
        </div>
      )}
    </Card>
  );
}
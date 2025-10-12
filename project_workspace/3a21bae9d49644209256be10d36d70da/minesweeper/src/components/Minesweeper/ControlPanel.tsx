'use client';

import React from 'react';
import { GameState, Difficulty } from '@/types/minesweeper';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue
} from '@/components/ui/select';
import { RotateCcw, Trophy, Skull } from 'lucide-react';

interface ControlPanelProps {
  gameState: GameState;
  remainingMines: number;
  timeElapsed: number;
  difficulty: Difficulty;
  onRestart: () => void;
  onDifficultyChange: (difficulty: Difficulty) => void;
}

export const MinesweeperControlPanel: React.FC<ControlPanelProps> = ({
  gameState,
  remainingMines,
  timeElapsed,
  difficulty,
  onRestart,
  onDifficultyChange
}) => {
  const formatTime = (seconds: number): string => {
    const mins = Math.floor(seconds / 60);
    const secs = seconds % 60;
    return `${mins.toString().padStart(2, '0')}:${secs.toString().padStart(2, '0')}`;
  };

  const getGameStateDisplay = () => {
    switch (gameState) {
      case 'idle':
        return { text: '准备开始', color: 'bg-gray-500', icon: null };
      case 'playing':
        return { text: '游戏中', color: 'bg-blue-500', icon: null };
      case 'won':
        return { text: '胜利！', color: 'bg-green-500', icon: <Trophy className="w-4 h-4" /> };
      case 'lost':
        return { text: '游戏结束', color: 'bg-red-500', icon: <Skull className="w-4 h-4" /> };
      default:
        return { text: '未知', color: 'bg-gray-500', icon: null };
    }
  };

  const gameStatus = getGameStateDisplay();

  return (
    <Card className="p-6 mb-6">
      <div className="flex items-center justify-between mb-4">
        <div className="flex items-center space-x-4">
          {/* 剩余地雷数 */}
          <div className="flex items-center space-x-2">
            <span className="text-2xl">💣</span>
            <div className="bg-black text-red-500 px-3 py-1 rounded font-mono text-xl font-bold min-w-[3rem] text-center">
              {Math.max(0, remainingMines).toString().padStart(3, '0')}
            </div>
          </div>

          {/* 游戏状态 */}
          <Badge className={`${gameStatus.color} text-white px-3 py-1 flex items-center space-x-1`}>
            {gameStatus.icon}
            <span>{gameStatus.text}</span>
          </Badge>

          {/* 计时器 */}
          <div className="flex items-center space-x-2">
            <span className="text-2xl">⏱️</span>
            <div className="bg-black text-green-400 px-3 py-1 rounded font-mono text-xl font-bold min-w-[3rem] text-center">
              {formatTime(timeElapsed)}
            </div>
          </div>
        </div>

        {/* 控制按钮 */}
        <div className="flex items-center space-x-2">
          {/* 难度选择 */}
          <Select value={difficulty} onValueChange={onDifficultyChange}>
            <SelectTrigger className="w-32">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="beginner">初级</SelectItem>
              <SelectItem value="intermediate">中级</SelectItem>
              <SelectItem value="expert">高级</SelectItem>
            </SelectContent>
          </Select>

          {/* 重新开始按钮 */}
          <Button
            onClick={onRestart}
            variant="outline"
            size="sm"
            className="flex items-center space-x-1"
          >
            <RotateCcw className="w-4 h-4" />
            <span>重新开始</span>
          </Button>
        </div>
      </div>

      {/* 游戏说明 */}
      <div className="text-sm text-gray-600 border-t pt-3">
        <div className="flex justify-between">
          <div>
            <strong>左键点击：</strong>揭开格子
          </div>
          <div>
            <strong>右键点击：</strong>标记旗帜/问号
          </div>
          <div>
            <strong>难度：</strong>
            {difficulty === 'beginner' && '初级 (9×9, 10雷)'}
            {difficulty === 'intermediate' && '中级 (16×16, 40雷)'}
            {difficulty === 'expert' && '高级 (16×30, 99雷)'}
          </div>
        </div>
      </div>
    </Card>
  );
};
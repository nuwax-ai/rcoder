#!/usr/bin/env python3
# -*- coding: utf-8 -*-

"""
平方计算器
这个脚本演示了Python的基本输入输出操作和数学计算功能
功能：
1. 读取用户输入的数字
2. 计算该数字的平方
3. 输出计算结果
"""

def calculate_square(number):
    """
    计算给定数字的平方

    参数:
        number (float/int): 要计算平方的数字

    返回:
        float: 输入数字的平方值
    """
    return number ** 2

def get_user_input():
    """
    获取用户输入的数字

    返回:
        float: 用户输入的数字

    异常:
        ValueError: 当用户输入无法转换为数字时抛出
    """
    try:
        # 提示用户输入数字
        user_input = input("请输入一个数字: ")
        # 将输入转换为浮点数，这样可以处理小数
        number = float(user_input)
        return number
    except ValueError:
        print(f"错误: '{user_input}' 不是一个有效的数字!")
        print("请确保输入的是数字，例如: 5, 3.14, -2 等")
        return None

def main():
    """
    主函数，协调整个程序的执行流程
    """
    print("=" * 40)
    print("           平方计算器")
    print("=" * 40)
    print("本程序将计算您输入数字的平方值")
    print()

    # 获取用户输入
    number = get_user_input()

    # 检查输入是否有效
    if number is None:
        print("程序终止：无效的输入")
        return

    # 计算平方
    result = calculate_square(number)

    # 输出结果
    print()
    print("=" * 40)
    print("计算结果:")
    print(f"输入数字: {number}")
    print(f"平方值: {result}")

    # 添加一些额外的有用信息
    if number == 0:
        print("提示: 0的平方仍然是0")
    elif number == 1:
        print("提示: 1的平方仍然是1")
    elif number > 0:
        print(f"提示: {number} * {number} = {result}")
    else:
        print(f"提示: {number} * {number} = {result} (负数的平方是正数)")

    print("=" * 40)
    print("程序执行完成!")

# 程序入口点
if __name__ == "__main__":
    main()